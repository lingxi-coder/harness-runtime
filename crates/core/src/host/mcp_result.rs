//! Ordinary modern MCP result decoding and local input-required fulfilment.
//! The transport and the registered callback authority stay on one connection.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::{stream::FuturesUnordered, StreamExt};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::time::Instant;
/// Cancellation scope shared by the driver and locally registered handlers.
pub use tokio_util::sync::CancellationToken;

/// An SDK result error, retaining the native code and structured data.
#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
pub struct McpSdkError {
    pub code: &'static str,
    pub message: String,
    pub data: Option<Value>,
}
impl McpSdkError {
    fn new(code: &'static str, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

#[derive(Debug, Error)]
pub enum McpResultError {
    #[error(transparent)]
    Connection(#[from] jsonrpc::ConnectionError),
    #[error(transparent)]
    Sdk(#[from] McpSdkError),
    #[error("local handler: code={}, message={}", .0.code, .0.message)]
    Local(jsonrpc::ResponseError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DecodedMcpResult {
    Complete(Value),
    InputRequired {
        input_requests: Map<String, Value>,
        request_state: Option<String>,
    },
    Task(Value),
}

/// Canonicalize integral JSON doubles that JavaScript represents as safe
/// integers. This changes neither map order nor arbitrary integer precision;
/// non-integral and out-of-range numbers retain their existing representation.
pub fn normalize_mcp_safe_integral_doubles(value: &mut Value) {
    match value {
        Value::Number(number) if number.is_f64() => {
            if let Some(value) = number
                .as_f64()
                .filter(|v| v.abs() <= 9_007_199_254_740_991.0 && v.fract() == 0.0)
            {
                *number = serde_json::Number::from(value as i64);
            }
        }
        Value::Array(array) => array
            .iter_mut()
            .for_each(normalize_mcp_safe_integral_doubles),
        Value::Object(object) => object
            .values_mut()
            .for_each(normalize_mcp_safe_integral_doubles),
        _ => {}
    }
}

fn invalid(method: &str, reason: &str, data: Value) -> McpSdkError {
    McpSdkError::new(
        "INVALID_RESULT",
        format!("Invalid result for {method}: {reason}"),
        Some(data),
    )
}

/// Decode the actual 2026 envelope. Input entry schema checks happen when
/// dispatching, so state-only replies and malformed entry values remain distinct.
pub fn decode_modern_result(
    method: &str,
    mut value: Value,
) -> Result<DecodedMcpResult, McpSdkError> {
    normalize_mcp_safe_integral_doubles(&mut value);
    let object = value
        .as_object()
        .ok_or_else(|| invalid(method, "not an object", json!({"method":method})))?;
    let tag = object.get("resultType").ok_or_else(|| invalid(method,
        "missing required resultType — servers implementing protocol revision 2026-07-28 MUST include it (the absent-means-complete bridge applies only to earlier-revision servers)",
        json!({"method":method,"violation":"missing-resultType"})))?;
    let tag = tag.as_str().ok_or_else(|| {
        invalid(
            method,
            "non-string resultType",
            json!({"method":method,"resultType":tag}),
        )
    })?;
    match tag {
        "complete" => {
            validate_complete(method, &value)
                .map_err(|reason| invalid(method, &reason, json!({"method":method})))?;
            value
                .as_object_mut()
                .expect("checked object")
                .shift_remove("resultType");
            Ok(DecodedMcpResult::Complete(value))
        }
        "input_required" => {
            let input_requests = object
                .get("inputRequests")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let request_state = object
                .get("requestState")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if input_requests.is_empty() && request_state.is_none() {
                return Err(invalid(method, "input_required carries neither inputRequests nor requestState (every input_required result must include at least one of the two)",
                    json!({"method":method,"violation":"input-required-missing-both"})));
            }
            Ok(DecodedMcpResult::InputRequired {
                input_requests,
                request_state,
            })
        }
        "task" => Ok(DecodedMcpResult::Task(value)),
        tag => Err(McpSdkError::new(
            "UNSUPPORTED_RESULT_TYPE",
            format!("Unsupported result type '{tag}' for {method}"),
            Some(json!({"resultType":tag,"method":method})),
        )),
    }
}

fn type_name(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}
fn type_issue(expected: &str, path: Vec<Value>, value: Option<&Value>) -> String {
    serde_json::to_string_pretty(&json!([{"expected":expected,"code":"invalid_type","path":path,"message":format!("Invalid input: expected {expected}, received {}", type_name(value))}])).expect("serializable schema issue")
}
fn require<'a>(
    object: &'a Value,
    key: &str,
    expected: &str,
    prefix: &[&str],
) -> Result<&'a Value, String> {
    let value = object.get(key);
    let good = value.is_some_and(|v| match expected {
        "array" => v.is_array(),
        "object" => v.is_object(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        _ => false,
    });
    if good {
        Ok(value.expect("present"))
    } else {
        let mut path: Vec<Value> = prefix.iter().map(|v| json!(v)).collect();
        path.push(json!(key));
        Err(type_issue(expected, path, value))
    }
}
fn optional_type(object: &Value, key: &str, expected: &str) -> Result<(), String> {
    if object.get(key).is_some() {
        require(object, key, expected, &[])?;
    }
    Ok(())
}
fn optional_fields(
    value: &Value,
    strings: &[&str],
    numbers: &[&str],
    booleans: &[&str],
) -> Result<(), String> {
    for key in strings {
        optional_type(value, key, "string")?;
    }
    for key in numbers {
        optional_type(value, key, "number")?;
    }
    for key in booleans {
        optional_type(value, key, "boolean")?;
    }
    Ok(())
}
fn base64_valid(text: &str) -> bool {
    let data: Vec<u8> = text
        .bytes()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 12))
        .collect();
    let unpadded = data.iter().position(|b| *b == b'=').unwrap_or(data.len());
    let padding = data.len() - unpadded;
    data[..unpadded]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
        && data[unpadded..].iter().all(|b| *b == b'=')
        && padding <= 2
        && (if padding > 0 {
            data.len().is_multiple_of(4) && unpadded % 4 + padding == 4
        } else {
            unpadded % 4 != 1
        })
}
fn iso_datetime_valid(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    let date_parts: Vec<_> = date.split('-').collect();
    if date_parts.len() != 3
        || date_parts[0].len() != 4
        || date_parts[1].len() != 2
        || date_parts[2].len() != 2
    {
        return false;
    }
    let parse = |v: &str| {
        v.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| v.parse::<u32>().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day)) = (
        parse(date_parts[0]),
        parse(date_parts[1]),
        parse(date_parts[2]),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    if day == 0 || day > days {
        return false;
    }
    let time = if let Some(time) = time.strip_suffix('Z') {
        time
    } else {
        let Some(offset) = time.find(['+', '-']) else {
            return false;
        };
        let zone = &time[offset + 1..];
        let parts: Vec<_> = zone.split(':').collect();
        if parts.len() != 2
            || parts[0].len() != 2
            || parts[1].len() != 2
            || parse(parts[0]).is_none_or(|v| v > 23)
            || parse(parts[1]).is_none_or(|v| v > 59)
        {
            return false;
        }
        &time[..offset]
    };
    let fields: Vec<_> = time.split(':').collect();
    if !(2..=3).contains(&fields.len())
        || fields[0].len() != 2
        || fields[1].len() != 2
        || parse(fields[0]).is_none_or(|v| v > 23)
        || parse(fields[1]).is_none_or(|v| v > 59)
    {
        return false;
    }
    if fields.len() == 3 {
        let (seconds, fraction) = fields[2]
            .split_once('.')
            .map_or((fields[2], None), |(s, f)| (s, Some(f)));
        if seconds.len() != 2
            || parse(seconds).is_none_or(|v| v > 59)
            || fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
        {
            return false;
        }
    }
    true
}
fn annotations_valid(value: &Value) -> bool {
    value.is_object()
        && value.get("audience").is_none_or(|a| {
            a.as_array().is_some_and(|a| {
                a.iter()
                    .all(|v| matches!(v.as_str(), Some("user" | "assistant")))
            })
        })
        && value
            .get("priority")
            .is_none_or(|v| v.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)))
        && value
            .get("lastModified")
            .is_none_or(|value| value.as_str().is_some_and(iso_datetime_valid))
}
fn icons_valid(value: &Value) -> bool {
    value.as_array().is_some_and(|icons| {
        icons.iter().all(|icon| {
            icon.get("src").is_some_and(Value::is_string)
                && optional_fields(icon, &["mimeType"], &[], &[]).is_ok()
                && icon
                    .get("sizes")
                    .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
                && icon
                    .get("theme")
                    .is_none_or(|v| matches!(v.as_str(), Some("light" | "dark")))
        })
    })
}
fn common_entry_valid(value: &Value) -> bool {
    value.is_object()
        && value.get("name").is_some_and(Value::is_string)
        && optional_fields(value, &["title", "description"], &[], &[]).is_ok()
        && value.get("icons").is_none_or(icons_valid)
        && value.get("_meta").is_none_or(Value::is_object)
}
fn resource_contents_valid(value: &Value) -> bool {
    value.is_object()
        && value.get("uri").is_some_and(Value::is_string)
        && optional_fields(value, &["mimeType"], &[], &[]).is_ok()
        && value.get("_meta").is_none_or(Value::is_object)
        && (value.get("text").is_some_and(Value::is_string)
            || value
                .get("blob")
                .and_then(Value::as_str)
                .is_some_and(base64_valid))
}
fn resource_entry_valid(value: &Value, template: bool) -> bool {
    common_entry_valid(value)
        && value
            .get(if template { "uriTemplate" } else { "uri" })
            .is_some_and(Value::is_string)
        && optional_fields(value, &["mimeType"], &["size"], &[]).is_ok()
        && value.get("annotations").is_none_or(annotations_valid)
}
fn content_valid(value: &Value) -> bool {
    if !value.is_object()
        || !value.get("annotations").is_none_or(annotations_valid)
        || !value.get("_meta").is_none_or(Value::is_object)
    {
        return false;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("text") => value.get("text").is_some_and(Value::is_string),
        Some("image" | "audio") => {
            value
                .get("data")
                .and_then(Value::as_str)
                .is_some_and(base64_valid)
                && value.get("mimeType").is_some_and(Value::is_string)
        }
        Some("resource_link") => resource_entry_valid(value, false),
        Some("resource") => value.get("resource").is_some_and(resource_contents_valid),
        _ => false,
    }
}
fn sampling_content_valid(value: &Value) -> bool {
    if !value.is_object() || !value.get("_meta").is_none_or(Value::is_object) {
        return false;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("text" | "image" | "audio") => content_valid(value),
        Some("tool_use") => {
            value.get("id").is_some_and(Value::is_string)
                && value.get("name").is_some_and(Value::is_string)
                && value.get("input").is_some_and(Value::is_object)
        }
        Some("tool_result") => {
            value.get("toolUseId").is_some_and(Value::is_string)
                && value
                    .get("content")
                    .is_some_and(|v| v.as_array().is_some_and(|v| v.iter().all(content_valid)))
                && optional_type(value, "isError", "boolean").is_ok()
        }
        _ => false,
    }
}
fn tool_entry_valid(value: &Value) -> bool {
    common_entry_valid(value)
        && value.get("inputSchema").is_some_and(|v| {
            v.is_object()
                && v.get("type").and_then(Value::as_str) == Some("object")
                && optional_type(v, "$schema", "string").is_ok()
        })
        && value
            .get("outputSchema")
            .is_none_or(|v| v.is_object() && optional_type(v, "$schema", "string").is_ok())
        && value.get("annotations").is_none_or(|v| {
            v.is_object()
                && optional_fields(
                    v,
                    &["title"],
                    &[],
                    &[
                        "readOnlyHint",
                        "destructiveHint",
                        "idempotentHint",
                        "openWorldHint",
                    ],
                )
                .is_ok()
        })
}
fn prompt_entry_valid(value: &Value) -> bool {
    common_entry_valid(value)
        && value.get("arguments").is_none_or(|v| {
            v.as_array().is_some_and(|v| {
                v.iter().all(|a| {
                    a.get("name").is_some_and(Value::is_string)
                        && optional_fields(a, &["description"], &[], &["required"]).is_ok()
                })
            })
        })
}
fn validate_complete(method: &str, value: &Value) -> Result<(), String> {
    // hn() has no schema for extension methods. Its complete result is copied
    // without schema coercion, exactly as for the native directory extension.
    if !matches!(
        method,
        "tools/call"
            | "tools/list"
            | "prompts/list"
            | "prompts/get"
            | "resources/list"
            | "resources/templates/list"
            | "resources/read"
            | "completion/complete"
            | "server/discover"
    ) {
        return Ok(());
    }
    optional_type(value, "_meta", "object")?;
    if matches!(
        method,
        "tools/list"
            | "prompts/list"
            | "resources/list"
            | "resources/templates/list"
            | "resources/read"
    ) {
        let ttl = require(value, "ttlMs", "number", &[])?;
        if !ttl.as_f64().is_some_and(|v| {
            (0.0..=9_007_199_254_740_991.0).contains(&v)
                && v.abs() <= 9_007_199_254_740_991.0
                && v.fract() == 0.0
        }) {
            return Err("ttlMs must be a non-negative integer".into());
        }
        require(value, "cacheScope", "string", &[])?;
        if !matches!(value["cacheScope"].as_str(), Some("public" | "private")) {
            return Err("Invalid cacheScope".into());
        }
    }
    let (field, entry): (&str, fn(&Value) -> bool) = match method {
        "tools/call" => ("content", content_valid),
        "tools/list" => ("tools", tool_entry_valid),
        "prompts/list" => ("prompts", prompt_entry_valid),
        "prompts/get" => ("messages", |m| {
            matches!(
                m.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) && m.get("content").is_some_and(content_valid)
        }),
        "resources/list" => ("resources", |v| resource_entry_valid(v, false)),
        "resources/templates/list" => ("resourceTemplates", |v| resource_entry_valid(v, true)),
        "resources/read" => ("contents", resource_contents_valid),
        _ => ("", |_| true),
    };
    if !field.is_empty() {
        let array = require(value, field, "array", &[])?
            .as_array()
            .expect("array");
        if array.iter().any(|v| !entry(v)) {
            return Err(format!("Invalid {field} entry"));
        }
    }
    match method {
        "tools/call" => optional_type(value, "isError", "boolean")?,
        "prompts/get" => optional_type(value, "description", "string")?,
        "completion/complete" => {
            let completion = require(value, "completion", "object", &[])?;
            let values = require(completion, "values", "array", &["completion"])?;
            if !values
                .as_array()
                .is_some_and(|v| v.len() <= 100 && v.iter().all(Value::is_string))
            {
                return Err("Invalid completion values".into());
            }
            optional_type(completion, "hasMore", "boolean")?;
            optional_type(completion, "total", "number")?;
            if completion.get("total").is_some_and(|v| {
                !v.as_f64()
                    .is_some_and(|v| v.abs() <= 9_007_199_254_740_991.0 && v.fract() == 0.0)
            }) {
                return Err("Invalid completion total".into());
            }
        }
        "server/discover" => {
            let versions = require(value, "supportedVersions", "array", &[])?;
            if !versions
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string))
            {
                return Err("Invalid supported versions".into());
            }
            require(value, "capabilities", "object", &[])?;
            optional_type(value, "instructions", "string")?;
        }
        _ => {}
    }
    if matches!(
        method,
        "tools/list" | "prompts/list" | "resources/list" | "resources/templates/list"
    ) {
        optional_type(value, "nextCursor", "string")?;
    }
    Ok(())
}

/// JavaScript Object.keys orders canonical uint32 indices (except 2^32-1)
/// numerically, before all other keys in their original insertion order.
pub fn javascript_object_keys(object: &Map<String, Value>) -> Vec<String> {
    let mut indices = Vec::new();
    let mut rest = Vec::new();
    for key in object.keys() {
        match key
            .parse::<u32>()
            .ok()
            .filter(|n| *n != u32::MAX && n.to_string() == *key)
        {
            Some(index) => indices.push((index, key.clone())),
            None => rest.push(key.clone()),
        }
    }
    indices.sort_unstable_by_key(|(n, _)| *n);
    indices
        .into_iter()
        .map(|(_, key)| key)
        .chain(rest)
        .collect()
}

/// Keep the original request, replacing only the current round's supplied fields.
pub fn rebuild_input_params(
    original: &Value,
    responses: Map<String, Value>,
    state: Option<&str>,
) -> Value {
    if responses.is_empty() && state.is_none() {
        return original.clone();
    }
    let mut object = original.as_object().cloned().unwrap_or_default();
    if !responses.is_empty() {
        object.insert("inputResponses".into(), Value::Object(responses));
    }
    if let Some(state) = state {
        object.insert("requestState".into(), json!(state));
    }
    Value::Object(object)
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpResultProgress {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}
pub type McpResultProgressCallback = Arc<dyn Fn(McpResultProgress) + Send + Sync>;
#[derive(Clone)]
pub struct McpInputRequiredOptions {
    pub auto_fulfill: bool,
    pub max_rounds: usize,
    pub per_request_timeout: Duration,
    pub max_total_timeout: Option<Duration>,
    pub reset_timeout_on_progress: bool,
    pub cancellation: CancellationToken,
    pub on_progress: Option<McpResultProgressCallback>,
}
impl Default for McpInputRequiredOptions {
    fn default() -> Self {
        Self {
            auto_fulfill: true,
            max_rounds: 10,
            per_request_timeout: Duration::from_secs(60),
            max_total_timeout: None,
            reset_timeout_on_progress: false,
            cancellation: CancellationToken::new(),
            on_progress: None,
        }
    }
}
#[async_trait]
pub trait McpInputRequiredIo: Send + Sync {
    async fn request(
        &self,
        method: &str,
        params: Value,
        options: &McpInputRequiredOptions,
    ) -> Result<Value, McpResultError>;
    async fn dispatch_local(
        &self,
        key: &str,
        request: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, McpResultError>;
}
fn cancelled() -> McpResultError {
    McpSdkError::new("REQUEST_TIMEOUT", "Request cancelled", None).into()
}
fn total_timeout(max: Duration, elapsed: Duration) -> McpResultError {
    McpSdkError::new(
        "REQUEST_TIMEOUT",
        "Maximum total timeout exceeded",
        Some(json!({"maxTotalTimeout":max.as_millis(),"totalElapsed":elapsed.as_millis()})),
    )
    .into()
}

/// The public Client.request result is parsed through native gt: its known
/// metadata field precedes passthrough fields, unlike decodeResult's shallow copy.
fn project_public_result(value: Value) -> Value {
    let object = value.as_object().expect("decoded complete object");
    let mut result = Map::new();
    if let Some(meta) = object.get("_meta") {
        if let Some(meta) = meta.as_object() {
            const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
            let mut projected = Map::new();
            if let Some(info) = meta.get(SERVER_INFO) {
                if info.get("name").is_some_and(Value::is_string)
                    && info.get("version").is_some_and(Value::is_string)
                    && optional_fields(info, &["title", "websiteUrl", "description"], &[], &[])
                        .is_ok()
                    && info.get("icons").is_none_or(icons_valid)
                {
                    let mut info = project_fields(
                        info,
                        &[
                            "name",
                            "title",
                            "icons",
                            "version",
                            "websiteUrl",
                            "description",
                        ],
                    );
                    if let Some(icons) = info.get("icons").cloned() {
                        info["icons"] = project_icons(&icons);
                    }
                    projected.insert(SERVER_INFO.to_owned(), info);
                }
            }
            for (key, value) in meta {
                if key != SERVER_INFO {
                    projected.insert(key.clone(), value.clone());
                }
            }
            result.insert("_meta".into(), Value::Object(projected));
        } else {
            result.insert("_meta".into(), meta.clone());
        }
    }
    for (key, value) in object {
        if key != "_meta" {
            result.insert(key.clone(), value.clone());
        }
    }
    Value::Object(result)
}

/// Run the ordinary automatic flow. The total budget is checked after local
/// handlers settle; it is not an extra deadline cutting off human elicitation.
pub async fn drive_modern_request(
    io: &dyn McpInputRequiredIo,
    method: &str,
    original: Value,
    options: McpInputRequiredOptions,
) -> Result<Value, McpResultError> {
    let started = Instant::now();
    let mut wire_options = options.clone();
    let mut result = io.request(method, original.clone(), &wire_options).await?;
    for round in 1.. {
        let (requests, state) = match decode_modern_result(method, result)? {
            DecodedMcpResult::Complete(value) => return Ok(project_public_result(value)),
            DecodedMcpResult::Task(_) => {
                return Err(McpSdkError::new(
                    "UNSUPPORTED_RESULT_TYPE",
                    format!("Unsupported result type 'task' for {method}"),
                    Some(json!({"method":method})),
                )
                .into())
            }
            DecodedMcpResult::InputRequired {
                input_requests,
                request_state,
            } => (input_requests, request_state),
        };
        if !options.auto_fulfill {
            return Err(McpSdkError::new("UNSUPPORTED_RESULT_TYPE",format!("Unsupported result type 'input_required' for {method}: multi-round-trip auto-fulfilment is not enabled on this instance — pass allowInputRequired: true to handle it manually, or enable inputRequired.autoFulfill"),Some(json!({"resultType":"input_required","method":method}))).into());
        }
        if round > options.max_rounds {
            let mut last = json!({"inputRequests":requests});
            if let Some(state) = &state {
                last["requestState"] = json!(state);
            }
            return Err(McpSdkError::new("INPUT_REQUIRED_ROUNDS_EXCEEDED",format!("Multi-round-trip request '{method}' still required input after {} rounds (inputRequired.maxRounds)",options.max_rounds),Some(json!({"rounds":options.max_rounds,"lastResult":last}))).into());
        }
        if let Some(progress) = &options.on_progress {
            progress(McpResultProgress {
                progress: round as f64,
                total: None,
                message: Some(format!(
                    "Fulfilling input required by '{method}' (round {round})"
                )),
            });
        }
        let keys = javascript_object_keys(&requests);
        let mut responses = Map::new();
        if keys.is_empty() {
            tokio::select! { _=options.cancellation.cancelled()=>return Err(cancelled()), _=tokio::time::sleep(Duration::from_millis(250))=>{} }
        } else {
            let scope = options.cancellation.child_token();
            let _scope_guard = scope.clone().drop_guard();
            let mut pending = FuturesUnordered::new();
            for key in &keys {
                let request = requests[key].clone();
                let cancellation = scope.clone();
                pending.push(async move {
                    let result = io.dispatch_local(key, request, cancellation).await;
                    // Promise.all starts every callback before observing even
                    // a synchronous failure from one of those callbacks.
                    tokio::task::yield_now().await;
                    (key.clone(), result)
                });
            }
            let mut completed = Map::new();
            loop {
                let next = tokio::select! { biased; _=scope.cancelled()=>return Err(cancelled()), next=pending.next()=>next };
                let Some((key, outcome)) = next else {
                    break;
                };
                match outcome {
                    Ok(value) => {
                        completed.insert(key, value);
                    }
                    Err(error) => {
                        scope.cancel();
                        return Err(error);
                    }
                }
            }
            drop(pending);
            for key in keys {
                responses.insert(
                    key.clone(),
                    completed.remove(&key).expect("all requests completed"),
                );
            }
        }
        if let Some(max) = options.max_total_timeout {
            let elapsed = started.elapsed();
            if elapsed >= max {
                return Err(total_timeout(max, elapsed));
            }
            wire_options.max_total_timeout = Some(max - elapsed);
        }
        let params = rebuild_input_params(&original, responses, state.as_deref());
        result = io.request(method, params, &wire_options).await?;
    }
    unreachable!("unbounded round loop")
}

/// The same JSON-RPC connection supplies transport I/O and its trusted local
/// registry. HTTP headers stay transport-owned and are preserved on every retry.
pub struct JsonrpcMcpResultIo {
    pub connection: Arc<jsonrpc::Connection>,
    pub client_capabilities: Value,
}
struct CancelRequest {
    connection: Arc<jsonrpc::Connection>,
    id: jsonrpc::Id,
    armed: bool,
    reason: String,
    meta: Option<Value>,
}
impl Drop for CancelRequest {
    fn drop(&mut self) {
        if self.armed && !self.connection.has_per_request_cancellation() {
            let _ = self.connection.notify("notifications/cancelled", {
                let mut params = json!({"requestId":self.id,"reason":self.reason});
                if let Some(meta) = &self.meta {
                    params["_meta"] = meta.clone();
                }
                params
            });
        }
    }
}

fn protocol_invalid(message: String) -> McpResultError {
    McpResultError::Local(jsonrpc::ResponseError {
        code: jsonrpc::INVALID_PARAMS,
        message,
        data: None,
    })
}
fn string_array(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|a| a.iter().all(Value::is_string))
}
fn elicitation_property_valid(value: &Value) -> bool {
    if !value.is_object() || optional_fields(value, &["title", "description"], &[], &[]).is_err() {
        return false;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("boolean") => value.get("default").is_none_or(Value::is_boolean),
        Some("number" | "integer") => {
            optional_fields(value, &[], &["minimum", "maximum", "default"], &[]).is_ok()
        }
        Some("string") => {
            let default_valid = optional_type(value, "default", "string").is_ok();
            let plain = optional_fields(value, &[], &["minLength", "maxLength"], &[]).is_ok()
                && value.get("format").is_none_or(|v| {
                    matches!(v.as_str(), Some("email" | "uri" | "date" | "date-time"))
                });
            let enumeration = value.get("enum").is_some_and(string_array);
            let choices = value.get("oneOf").is_some_and(|v| {
                v.as_array().is_some_and(|a| {
                    a.iter().all(|v| {
                        v.get("const").is_some_and(Value::is_string)
                            && v.get("title").is_some_and(Value::is_string)
                    })
                })
            });
            default_valid && (plain || enumeration || choices)
        }
        Some("array") => {
            optional_fields(value, &[], &["minItems", "maxItems"], &[]).is_ok()
                && value.get("default").is_none_or(string_array)
                && value.get("items").is_some_and(|items| {
                    items.is_object()
                        && ((items.get("type").and_then(Value::as_str) == Some("string")
                            && items.get("enum").is_some_and(string_array))
                            || items.get("anyOf").is_some_and(|v| {
                                v.as_array().is_some_and(|a| {
                                    a.iter().all(|v| {
                                        v.get("const").is_some_and(Value::is_string)
                                            && v.get("title").is_some_and(Value::is_string)
                                    })
                                })
                            }))
                })
        }
        _ => false,
    }
}
fn sampling_params_valid(p: &Value) -> Result<(), String> {
    require(p, "messages", "array", &["params"])?;
    let max = require(p, "maxTokens", "number", &["params"])?;
    if !max
        .as_f64()
        .is_some_and(|v| v.abs() <= 9_007_199_254_740_991.0 && v.fract() == 0.0)
    {
        return Err("maxTokens must be an integer".into());
    }
    if !p["messages"].as_array().is_some_and(|messages| {
        messages.iter().all(|m| {
            matches!(
                m.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) && m.get("content").is_some_and(|c| {
                sampling_content_valid(c)
                    || c.as_array()
                        .is_some_and(|a| a.iter().all(sampling_content_valid))
            }) && m.get("_meta").is_none_or(Value::is_object)
        })
    }) {
        return Err("Invalid sampling messages".into());
    }
    optional_fields(p, &["systemPrompt"], &["temperature"], &[])?;
    if p.get("includeContext")
        .is_some_and(|v| !matches!(v.as_str(), Some("none" | "thisServer" | "allServers")))
        || p.get("stopSequences").is_some_and(|v| !string_array(v))
        || p.get("metadata").is_some_and(|v| !v.is_object())
        || p.get("tools")
            .is_some_and(|v| !v.as_array().is_some_and(|a| a.iter().all(tool_entry_valid)))
        || p.get("toolChoice").is_some_and(|v| {
            !v.is_object()
                || v.get("mode")
                    .is_some_and(|v| !matches!(v.as_str(), Some("auto" | "required" | "none")))
        })
    {
        return Err("Invalid sampling parameters".into());
    }
    if let Some(preferences) = p.get("modelPreferences") {
        if !preferences.is_object()
            || preferences.get("hints").is_some_and(|v| {
                !v.as_array().is_some_and(|a| {
                    a.iter()
                        .all(|v| v.is_object() && optional_type(v, "name", "string").is_ok())
                })
            })
            || ["costPriority", "speedPriority", "intelligencePriority"]
                .iter()
                .any(|key| {
                    preferences
                        .get(*key)
                        .is_some_and(|v| !v.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)))
                })
        {
            return Err("Invalid model preferences".into());
        }
    }
    Ok(())
}
fn elicitation_params_valid(p: &Value) -> Result<(), String> {
    require(p, "message", "string", &["params"])?;
    match p.get("mode").and_then(Value::as_str) {
        Some("url") => {
            let url = require(p, "url", "string", &["params"])?;
            if url::Url::parse(url.as_str().expect("string")).is_err() {
                return Err("Invalid URL".into());
            }
        }
        None | Some("form") => {
            if p.get("mode").is_some_and(|v| !v.is_string()) {
                return Err("Invalid elicitation mode".into());
            }
            let schema = require(p, "requestedSchema", "object", &["params"])?;
            if schema.get("type").and_then(Value::as_str) != Some("object")
                || !schema.get("properties").is_some_and(|v| {
                    v.as_object()
                        .is_some_and(|o| o.values().all(elicitation_property_valid))
                })
                || schema.get("required").is_some_and(|v| !string_array(v))
            {
                return Err("Invalid elicitation object schema".into());
            }
            if p.get("_meta").is_some_and(|v| !v.is_object())
                || p.get("task")
                    .is_some_and(|v| !v.is_object() || optional_type(v, "ttl", "number").is_err())
            {
                return Err("Invalid elicitation request metadata".into());
            }
        }
        Some(_) => return Err("Invalid elicitation mode".into()),
    }
    Ok(())
}

fn project_fields(value: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|key| {
                value
                    .get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect(),
    )
}
fn project_annotations(value: &Value) -> Value {
    project_fields(value, &["audience", "priority", "lastModified"])
}
fn project_icons(value: &Value) -> Value {
    Value::Array(
        value
            .as_array()
            .expect("validated icons")
            .iter()
            .map(|v| project_fields(v, &["src", "mimeType", "sizes", "theme"]))
            .collect(),
    )
}
fn project_content(value: &Value) -> Value {
    let fields: &[&str] = match value["type"].as_str().expect("validated block") {
        "text" => &["type", "text", "annotations", "_meta"],
        "image" | "audio" => &["type", "data", "mimeType", "annotations", "_meta"],
        "tool_use" => &["type", "name", "id", "input", "_meta"],
        "tool_result" => &[
            "type",
            "toolUseId",
            "content",
            "structuredContent",
            "isError",
            "_meta",
        ],
        "resource" => &["type", "resource", "annotations", "_meta"],
        "resource_link" => &[
            "name",
            "title",
            "icons",
            "uri",
            "description",
            "mimeType",
            "size",
            "annotations",
            "_meta",
            "type",
        ],
        _ => unreachable!("validated content type"),
    };
    let mut result = project_fields(value, fields);
    if let Some(annotations) = value.get("annotations") {
        result["annotations"] = project_annotations(annotations);
    }
    if let Some(icons) = value.get("icons") {
        result["icons"] = project_icons(icons);
    }
    if let Some(resource) = value.get("resource") {
        result["resource"] = project_fields(
            resource,
            if resource.get("text").is_some() {
                &["uri", "mimeType", "_meta", "text"]
            } else {
                &["uri", "mimeType", "_meta", "blob"]
            },
        );
    }
    if value["type"] == "tool_result" {
        result["content"] = Value::Array(
            value["content"]
                .as_array()
                .expect("validated content")
                .iter()
                .map(project_content)
                .collect(),
        );
    }
    result
}
fn project_message(value: &Value) -> Value {
    let mut result = project_fields(value, &["role", "content", "_meta"]);
    result["content"] = if let Some(array) = value["content"].as_array() {
        Value::Array(array.iter().map(project_content).collect())
    } else {
        project_content(&value["content"])
    };
    result
}
fn project_tool(value: &Value) -> Value {
    let mut result = project_fields(
        value,
        &[
            "name",
            "title",
            "icons",
            "description",
            "inputSchema",
            "outputSchema",
            "annotations",
            "_meta",
        ],
    );
    if let Some(annotations) = value.get("annotations") {
        result["annotations"] = project_fields(
            annotations,
            &[
                "title",
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ],
        );
    }
    if let Some(icons) = value.get("icons") {
        result["icons"] = project_icons(icons);
    }
    result
}
fn project_elicitation_property(value: &Value) -> Value {
    let kind = value["type"].as_str().expect("validated property");
    let fields: &[&str] = match kind {
        "boolean" => &["type", "title", "description", "default"],
        "number" | "integer" => &[
            "type",
            "title",
            "description",
            "minimum",
            "maximum",
            "default",
        ],
        "string" if value.get("enum").is_some_and(string_array) => {
            if value.get("enumNames").is_none_or(string_array) {
                &[
                    "type",
                    "title",
                    "description",
                    "enum",
                    "enumNames",
                    "default",
                ]
            } else {
                &["type", "title", "description", "enum", "default"]
            }
        }
        "string"
            if value.get("oneOf").is_some_and(|v| {
                v.as_array().is_some_and(|a| {
                    a.iter().all(|v| {
                        v.get("const").is_some_and(Value::is_string)
                            && v.get("title").is_some_and(Value::is_string)
                    })
                })
            }) =>
        {
            &["type", "title", "description", "oneOf", "default"]
        }
        "string" => &[
            "type",
            "title",
            "description",
            "minLength",
            "maxLength",
            "format",
            "default",
        ],
        "array" => &[
            "type",
            "title",
            "description",
            "minItems",
            "maxItems",
            "items",
            "default",
        ],
        _ => unreachable!("validated property"),
    };
    let mut result = project_fields(value, fields);
    if let Some(choices) = result.get_mut("oneOf") {
        *choices = Value::Array(
            choices
                .as_array()
                .expect("validated choices")
                .iter()
                .map(|v| project_fields(v, &["const", "title"]))
                .collect(),
        );
    }
    if kind == "array" {
        let items = &value["items"];
        result["items"] = if items.get("type").and_then(Value::as_str) == Some("string")
            && items.get("enum").is_some_and(string_array)
        {
            project_fields(items, &["type", "enum"])
        } else {
            json!({"anyOf":items["anyOf"].as_array().expect("validated choices").iter().map(|v|project_fields(v,&["const","title"])).collect::<Vec<_>>()})
        };
    }
    result
}
fn project_embedded_params(method: &str, value: &Value) -> Value {
    match method {
        "roots/list" => project_fields(value, &["_meta"]),
        "elicitation/create" => {
            let fields: &[&str] = if value["mode"] == "url" {
                &["mode", "message", "url"]
            } else {
                &["_meta", "task", "mode", "message", "requestedSchema"]
            };
            let mut result = project_fields(value, fields);
            if let Some(task) = value.get("task") {
                if result.get("task").is_some() {
                    result["task"] = project_fields(task, &["ttl"]);
                }
            }
            if let Some(schema) = value.get("requestedSchema") {
                if result.get("requestedSchema").is_some() {
                    let mut parsed = project_fields(schema, &["type", "properties", "required"]);
                    parsed["properties"] = Value::Object(
                        schema["properties"]
                            .as_object()
                            .expect("validated properties")
                            .iter()
                            .map(|(key, value)| (key.clone(), project_elicitation_property(value)))
                            .collect(),
                    );
                    for (key, value) in schema.as_object().expect("validated schema") {
                        if !["type", "properties", "required"].contains(&key.as_str()) {
                            parsed
                                .as_object_mut()
                                .expect("schema")
                                .insert(key.clone(), value.clone());
                        }
                    }
                    result["requestedSchema"] = parsed;
                }
            }
            result
        }
        "sampling/createMessage" => {
            let mut result = project_fields(
                value,
                &[
                    "messages",
                    "modelPreferences",
                    "systemPrompt",
                    "includeContext",
                    "temperature",
                    "maxTokens",
                    "stopSequences",
                    "metadata",
                    "tools",
                    "toolChoice",
                ],
            );
            result["messages"] = Value::Array(
                value["messages"]
                    .as_array()
                    .expect("validated messages")
                    .iter()
                    .map(project_message)
                    .collect(),
            );
            if let Some(preferences) = value.get("modelPreferences") {
                result["modelPreferences"] = project_fields(
                    preferences,
                    &[
                        "hints",
                        "costPriority",
                        "speedPriority",
                        "intelligencePriority",
                    ],
                );
                if let Some(hints) = preferences.get("hints") {
                    result["modelPreferences"]["hints"] = Value::Array(
                        hints
                            .as_array()
                            .expect("validated hints")
                            .iter()
                            .map(|v| project_fields(v, &["name"]))
                            .collect(),
                    );
                }
            }
            if let Some(tools) = value.get("tools") {
                result["tools"] = Value::Array(
                    tools
                        .as_array()
                        .expect("validated tools")
                        .iter()
                        .map(project_tool)
                        .collect(),
                );
            }
            if let Some(choice) = value.get("toolChoice") {
                result["toolChoice"] = project_fields(choice, &["mode"]);
            }
            result
        }
        _ => unreachable!("validated embedded method"),
    }
}

fn embedded_method<'a>(key: &str, value: &'a Value) -> Result<&'a str, McpResultError> {
    let method = value.as_object().and_then(|o|o.get("method")).and_then(Value::as_str).ok_or_else(|| McpSdkError::new("INVALID_RESULT",format!("Invalid input request '{key}': each inputRequests entry must be an embedded request object with a method"),Some(json!({"key":key}))))?;
    if !matches!(
        method,
        "roots/list" | "elicitation/create" | "sampling/createMessage"
    ) {
        return Err(McpSdkError::new("INVALID_RESULT",format!("Invalid input request '{key}': '{method}' is not an embedded request the 2026-07-28 revision defines (expected elicitation/create, sampling/createMessage, or roots/list)"),Some(json!({"key":key,"method":method}))).into());
    }
    Ok(method)
}
/// Validate the registered callback route and use the original key as id.
/// Elicitation and sampling use their native input schemas. The native common
/// roots wire handler retains object params, including unknown metadata fields.
pub fn validate_embedded_request(
    key: &str,
    value: &Value,
) -> Result<jsonrpc::Request, McpResultError> {
    let method = embedded_method(key, value)?;
    let mut params = value.get("params").filter(|v| v.is_object()).cloned();
    if let Some(params) = &mut params {
        normalize_mcp_safe_integral_doubles(params);
    }
    let p = params.as_ref().unwrap_or(&Value::Null);
    let validation = match method {
        "sampling/createMessage" => sampling_params_valid(p),
        "elicitation/create" => elicitation_params_valid(p),
        _ => Ok(()),
    };
    validation.map_err(|issue| {
        protocol_invalid(format!(
            "Invalid {} request: {issue}",
            if method == "sampling/createMessage" {
                "sampling"
            } else {
                "elicitation"
            }
        ))
    })?;
    if method != "roots/list" {
        if let Some(params) = &mut params {
            *params = project_embedded_params(method, params);
        }
    }
    Ok(jsonrpc::Request::new(
        method,
        params,
        jsonrpc::Id::String(key.to_owned()),
    ))
}
fn validate_local_result(method: &str, value: &Value) -> Result<(), McpResultError> {
    let issue: Result<(), String> = match method {
        "sampling/createMessage" => (|| {
            require(value, "model", "string", &[])?;
            optional_fields(value, &["stopReason"], &[], &[])?;
            optional_type(value, "_meta", "object")?;
            if !matches!(
                value.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) || !value.get("content").is_some_and(|v| {
                sampling_content_valid(v)
                    || v.as_array()
                        .is_some_and(|a| a.iter().all(sampling_content_valid))
            }) {
                return Err("Invalid sampling message".into());
            }
            Ok(())
        })(),
        "elicitation/create" => {
            if matches!(
                value.get("action").and_then(Value::as_str),
                Some("accept" | "decline" | "cancel")
            ) && value.get("content").is_none_or(|v| {
                v.as_object().is_some_and(|o| {
                    o.values().all(|v| {
                        v.is_string()
                            || v.is_number()
                            || v.is_boolean()
                            || v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
                    })
                })
            }) {
                Ok(())
            } else {
                Err("Invalid elicitation result".into())
            }
        }
        _ => Ok(()), // native roots callback results are passed through
    };
    issue.map_err(|issue| {
        protocol_invalid(format!(
            "Invalid {} result: {issue}",
            if method == "sampling/createMessage" {
                "sampling"
            } else {
                "elicitation"
            }
        ))
    })
}
#[async_trait]
impl McpInputRequiredIo for JsonrpcMcpResultIo {
    async fn request(
        &self,
        method: &str,
        params: Value,
        options: &McpInputRequiredOptions,
    ) -> Result<Value, McpResultError> {
        if options.cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let mut params = params;
        normalize_mcp_safe_integral_doubles(&mut params);
        let mut notifications = self.connection.notifications();
        let mut progress_enabled = options.on_progress.is_some();
        let cancellation_meta = params.get("_meta").and_then(Value::as_object).map(|meta| {
            Value::Object(
                meta.iter()
                    .filter(|(key, _)| key.starts_with("io.modelcontextprotocol/"))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
        });
        let call = self.connection.start_call_with_params(method, |id| {
            let mut params = params;
            if progress_enabled {
                let object = params
                    .as_object_mut()
                    .expect("MCP request parameters object");
                let meta = object.entry("_meta").or_insert_with(|| json!({}));
                if let Some(meta) = meta.as_object_mut() {
                    meta.insert("progressToken".into(), json!(id));
                }
            }
            params
        })?;
        let id = call.id().clone();
        let mut cancellation = CancelRequest {
            connection: self.connection.clone(),
            id: id.clone(),
            armed: true,
            reason: "Request cancelled".into(),
            meta: cancellation_meta,
        };
        let wait = call.wait_value();
        tokio::pin!(wait);
        let start = Instant::now();
        let mut deadline = start + options.per_request_timeout;
        loop {
            tokio::select! {
                biased;
                _=options.cancellation.cancelled()=>return Err(cancelled()),
                value=&mut wait=>{ cancellation.armed=false; return value.map_err(|e|McpResultError::Connection(e.into())); },
                _=tokio::time::sleep_until(deadline)=>{cancellation.reason="SdkError: Request timed out".into();return Err(McpSdkError::new("REQUEST_TIMEOUT","Request timed out",Some(json!({"timeout":options.per_request_timeout.as_millis()}))).into());},
                notice=notifications.recv(), if progress_enabled=>{
                    let notice=match notice {Ok(notice)=>notice,Err(tokio::sync::broadcast::error::RecvError::Closed)=>{progress_enabled=false;continue;},Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>continue};
                    if notice.method!="notifications/progress" {continue;}
                    let Some(p)=notice.params else {continue;}; if p.get("progressToken")!=Some(&json!(id)) {continue;}
                    if options.reset_timeout_on_progress {
                        if let Some(max)=options.max_total_timeout { if start.elapsed()>=max {cancellation.reason="SdkError: Maximum total timeout exceeded".into();return Err(total_timeout(max,start.elapsed()));} }
                        deadline=Instant::now()+options.per_request_timeout;
                    }
                    if let Some(callback)=&options.on_progress {callback(McpResultProgress {progress:p.get("progress").and_then(Value::as_f64).unwrap_or(0.0),total:p.get("total").and_then(Value::as_f64),message:p.get("message").and_then(Value::as_str).map(str::to_owned)});}
                }
            }
        }
    }
    async fn dispatch_local(
        &self,
        key: &str,
        value: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, McpResultError> {
        let method = embedded_method(key, &value)?;
        let handler=self.connection.local_registered_handler(method).await.ok_or_else(||McpSdkError::new("CAPABILITY_NOT_SUPPORTED",format!("Cannot fulfil input request '{key}': no handler is registered for '{method}' on this client. Declare the corresponding capability and register a handler, or handle input_required results manually."),Some(json!({"key":key,"method":method}))))?;
        let request = validate_embedded_request(key, &value)?;
        let capability = match request.method.as_str() {
            "roots/list" => "roots",
            "elicitation/create" => "elicitation",
            _ => "sampling",
        };
        if !self
            .client_capabilities
            .get(capability)
            .is_some_and(Value::is_object)
        {
            return Err(McpSdkError::new(
                "CAPABILITY_NOT_SUPPORTED",
                format!(
                    "Client does not support {capability} capability (required for {})",
                    request.method
                ),
                None,
            )
            .into());
        }
        if request.method == "elicitation/create" {
            let mode = request
                .params
                .as_ref()
                .and_then(|p| p.get("mode"))
                .and_then(Value::as_str)
                .unwrap_or("form");
            let caps = &self.client_capabilities["elicitation"];
            let supported = if mode == "url" {
                caps.get("url").is_some_and(Value::is_object)
            } else {
                caps.as_object().is_some_and(Map::is_empty)
                    || caps.get("form").is_some_and(Value::is_object)
            };
            if !supported {
                return Err(protocol_invalid(format!(
                    "Client does not support {mode}-mode elicitation requests"
                )));
            }
        }
        let method = request.method.clone();
        let response = tokio::select! { biased; _=cancellation.cancelled()=>return Err(cancelled()), response=handler.handle(request)=>response };
        if let Some(error) = response.error {
            return Err(McpResultError::Local(error));
        }
        let mut result = response.result.unwrap_or(Value::Null);
        validate_local_result(&method, &result)?;
        normalize_mcp_safe_integral_doubles(&mut result);
        let result = match method.as_str() {
            "roots/list" => result,
            "elicitation/create" => project_fields(&result, &["action", "content"]),
            "sampling/createMessage" => {
                let mut projected = project_message(&result);
                projected["model"] = result["model"].clone();
                if let Some(reason) = result.get("stopReason") {
                    projected["stopReason"] = reason.clone();
                }
                projected
            }
            _ => unreachable!("validated method"),
        };
        Ok(result)
    }
}

/// Validate the native embedded response union. Roots dispatch itself preserves
/// the registered handler result; this schema is used for explicit input maps.
pub fn validate_input_response_schema(value: &Value) -> bool {
    let roots = value.get("roots").is_some_and(|v| {
        v.as_array().is_some_and(|roots| {
            roots.iter().all(|r| {
                r.get("uri")
                    .and_then(Value::as_str)
                    .is_some_and(|uri| uri.starts_with("file://"))
                    && optional_fields(r, &["name"], &[], &[]).is_ok()
                    && r.get("_meta").is_none_or(Value::is_object)
            })
        })
    });
    roots
        || (value.get("model").is_some()
            && validate_local_result("sampling/createMessage", value).is_ok())
        || (value.get("action").is_some()
            && validate_local_result("elicitation/create", value).is_ok())
}
/// Validate the native InputRequiredResultSchema independently of decodeResult's
/// additional requirement that at least one input key or state be present.
pub fn validate_input_required_schema(value: &Value) -> bool {
    value.is_object()
        && value.get("resultType").is_none_or(Value::is_string)
        && value.get("_meta").is_none_or(Value::is_object)
        && value.get("requestState").is_none_or(Value::is_string)
        && value.get("inputRequests").is_none_or(|v| {
            v.as_object().is_some_and(|requests| {
                requests
                    .iter()
                    .all(|(_, value)| validate_input_request_schema(value))
            })
        })
}

/// Validate the native request union without the dispatcher's deliberate
/// coercion of a non-object params field into an omitted field.
pub fn validate_input_request_schema(value: &Value) -> bool {
    value.get("params").is_none_or(Value::is_object)
        && (value.get("method").and_then(Value::as_str) != Some("roots/list")
            || value
                .get("params")
                .and_then(|p| p.get("_meta"))
                .is_none_or(Value::is_object))
        && validate_embedded_request("schema", value).is_ok()
}
