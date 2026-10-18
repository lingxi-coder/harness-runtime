//! Modern MCP result decoding and local input-required fulfilment.
//!
//! Tool results and embedded requests stay in the flat UTF-16 JSON carrier
//! from the JSON-RPC response through validation, local dispatch, and the
//! retried request. Only locally generated diagnostics use ordinary Rust
//! strings.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::{stream::FuturesUnordered, StreamExt};
use thiserror::Error;
use tokio::time::Instant;

use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Cancellation scope shared by the driver and locally registered handlers.
pub use tokio_util::sync::CancellationToken;

pub use mcp_wire::transport::McpSdkError;

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
    Complete(Utf16JsonProjection),
    InputRequired {
        input_requests: Vec<SourceInputRequest>,
        request_state: Option<Utf16JsonProjection>,
    },
    Task(Utf16JsonProjection),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceInputRequest {
    pub key_units: Vec<u16>,
    pub request: Utf16JsonProjection,
}

fn string_value(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust string is valid UTF-16")
}

fn string_units_value(value: &[u16]) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(value), value.to_vec())
        .expect("display form is derived from the exact UTF-16 string")
}

fn null_value() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

fn bool_value(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" }).expect("boolean is valid JSON")
}

fn number_value(value: f64) -> Utf16JsonProjection {
    let mut source = serde_json::to_string(&value).expect("finite progress is serializable");
    if !value.is_finite() {
        source = "null".to_owned();
    }
    Utf16JsonProjection::parse(&source).expect("number is valid JSON")
}

fn source_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("generated object fields are unique")
}

fn root_string_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display form is derived from the exact UTF-16 string")
}

fn append_ascii(units: &mut Vec<u16>, text: &str) {
    units.extend(text.encode_utf16());
}

/// Build the JSON-RPC connection diagnostic without formatting a remote
/// message through `Display`, which would lose lone UTF-16 surrogates.
#[must_use]
pub fn connection_error_diagnostic(error: &jsonrpc::ConnectionError) -> Utf16JsonProjection {
    match error {
        jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(remote)) => {
            let mut units = "router: remote error: code="
                .encode_utf16()
                .collect::<Vec<_>>();
            append_ascii(&mut units, &remote.code.to_string());
            append_ascii(&mut units, ", message=");
            units.extend_from_slice(remote.message.code_units());
            root_string_units(units)
        }
        _ => error.to_string().into(),
    }
}

/// Source text corresponding to an MCP result failure. Remote JSON-RPC
/// messages stay exact; locally generated diagnostics are projected at their
/// Rust-string origin.
#[must_use]
pub fn result_error_diagnostic(error: &McpResultError) -> Utf16JsonProjection {
    match error {
        McpResultError::Connection(error) => connection_error_diagnostic(error),
        McpResultError::Sdk(error) => format!("{}: {}", error.code, error.message).into(),
        McpResultError::Local(error) => {
            let mut units = format!("local handler: code={}, message=", error.code)
                .encode_utf16()
                .collect::<Vec<_>>();
            units.extend_from_slice(error.message.code_units());
            root_string_units(units)
        }
    }
}

/// Apply the JavaScript binary64 normalization performed by the native MCP
/// result path. Parsing into the flat carrier already uses binary64 semantics,
/// and its JSON stringifier emits integral safe doubles without a `.0` suffix;
/// the stringify/parse pass makes that canonical form explicit while keeping
/// every UTF-16 string and object key exact.
pub fn normalize_mcp_safe_integral_doubles(value: Utf16JsonProjection) -> Utf16JsonProjection {
    let source = value
        .to_json_string()
        .expect("validated source projection is serializable");
    Utf16JsonProjection::parse(&source).expect("projection stringifies to valid JSON")
}

fn is_object(view: JsonValueView<'_>) -> bool {
    view.object_entries().is_some()
}

fn is_array(view: JsonValueView<'_>) -> bool {
    view.array_len().is_some()
}

fn is_integer(view: JsonValueView<'_>) -> bool {
    view.as_number().is_some_and(|value| {
        value.is_finite() && value.abs() <= 9_007_199_254_740_991.0 && value.fract() == 0.0
    })
}

fn is_one_of(view: JsonValueView<'_>, choices: &[&str]) -> bool {
    view.as_str().is_some_and(|value| choices.contains(&value))
        && view.code_units().is_some_and(|units| {
            choices
                .iter()
                .any(|choice| units == choice.encode_utf16().collect::<Vec<_>>())
        })
}

fn view_type_name(value: Option<JsonValueView<'_>>) -> &'static str {
    match value {
        None => "undefined",
        Some(value) if value.is_null() => "null",
        Some(value) if value.as_bool().is_some() => "boolean",
        Some(value) if value.as_number().is_some() => "number",
        Some(value) if value.as_str().is_some() => "string",
        Some(value) if value.array_len().is_some() => "array",
        Some(_) => "object",
    }
}

fn type_issue(expected: &str, path: &[&str], value: Option<JsonValueView<'_>>) -> String {
    serde_json::to_string_pretty(&serde_json::json!([{
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!(
            "Invalid input: expected {expected}, received {}",
            view_type_name(value)
        ),
    }]))
    .expect("generated schema issue is valid JSON")
}

fn required<'a>(
    object: JsonValueView<'a>,
    key: &str,
    expected: &str,
    path: &[&str],
) -> Result<JsonValueView<'a>, String> {
    let value = object.get_property(key);
    let valid = value.is_some_and(|value| match expected {
        "array" => is_array(value),
        "object" => is_object(value),
        "string" => value.as_str().is_some(),
        "number" => value.as_number().is_some(),
        "boolean" => value.as_bool().is_some(),
        _ => false,
    });
    if let Some(value) = value.filter(|_| valid) {
        return Ok(value);
    }
    let mut path = path.to_vec();
    path.push(key);
    Err(type_issue(expected, &path, value))
}

fn optional_type(object: JsonValueView<'_>, key: &str, expected: &str) -> Result<(), String> {
    if object.get_property(key).is_some() {
        required(object, key, expected, &[])?;
    }
    Ok(())
}

fn optional_types(
    object: JsonValueView<'_>,
    strings: &[&str],
    numbers: &[&str],
    booleans: &[&str],
) -> Result<(), String> {
    for key in strings {
        optional_type(object, key, "string")?;
    }
    for key in numbers {
        optional_type(object, key, "number")?;
    }
    for key in booleans {
        optional_type(object, key, "boolean")?;
    }
    Ok(())
}

fn array_all(view: JsonValueView<'_>, predicate: impl Fn(JsonValueView<'_>) -> bool) -> bool {
    let Some(length) = view.array_len() else {
        return false;
    };
    (0..length).all(|index| view.get_index(index).is_some_and(&predicate))
}

fn string_array(view: JsonValueView<'_>) -> bool {
    array_all(view, |item| item.as_str().is_some())
}

fn annotations_valid(value: JsonValueView<'_>) -> bool {
    is_object(value)
        && value.get_property("audience").is_none_or(|audience| {
            array_all(audience, |item| is_one_of(item, &["user", "assistant"]))
        })
        && value.get_property("priority").is_none_or(|priority| {
            priority
                .as_number()
                .is_some_and(|priority| (0.0..=1.0).contains(&priority))
        })
        && value.get_property("lastModified").is_none_or(|modified| {
            modified.as_str().is_some_and(iso_datetime_valid) && modified.code_units().is_some()
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
    let parse = |part: &str| {
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse::<u32>().ok())
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
        2 if leap => 29,
        2 => 28,
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
        let zone: Vec<_> = time[offset + 1..].split(':').collect();
        if zone.len() != 2
            || zone[0].len() != 2
            || zone[1].len() != 2
            || parse(zone[0]).is_none_or(|value| value > 23)
            || parse(zone[1]).is_none_or(|value| value > 59)
        {
            return false;
        }
        &time[..offset]
    };
    let fields: Vec<_> = time.split(':').collect();
    if !(2..=3).contains(&fields.len())
        || fields[0].len() != 2
        || fields[1].len() != 2
        || parse(fields[0]).is_none_or(|value| value > 23)
        || parse(fields[1]).is_none_or(|value| value > 59)
    {
        return false;
    }
    if fields.len() == 3 {
        let (seconds, fraction) = fields[2]
            .split_once('.')
            .map_or((fields[2], None), |(seconds, fraction)| {
                (seconds, Some(fraction))
            });
        if seconds.len() != 2
            || parse(seconds).is_none_or(|value| value > 59)
            || fraction.is_some_and(|fraction| {
                fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return false;
        }
    }
    true
}

fn icons_valid(value: JsonValueView<'_>) -> bool {
    array_all(value, |icon| {
        is_object(icon)
            && icon
                .get_property("src")
                .is_some_and(|src| src.as_str().is_some())
            && optional_types(icon, &["mimeType"], &[], &[]).is_ok()
            && icon.get_property("sizes").is_none_or(string_array)
            && icon
                .get_property("theme")
                .is_none_or(|theme| is_one_of(theme, &["light", "dark"]))
    })
}

fn base64_valid(text: &str) -> bool {
    let data: Vec<u8> = text
        .bytes()
        .filter(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 12))
        .collect();
    let unpadded = data
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(data.len());
    let padding = data.len() - unpadded;
    data[..unpadded]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        && data[unpadded..].iter().all(|byte| *byte == b'=')
        && padding <= 2
        && if padding > 0 {
            data.len().is_multiple_of(4) && unpadded % 4 + padding == 4
        } else {
            unpadded % 4 != 1
        }
}

fn common_entry_valid(value: JsonValueView<'_>) -> bool {
    is_object(value)
        && value
            .get_property("name")
            .is_some_and(|name| name.as_str().is_some())
        && optional_types(value, &["title", "description"], &[], &[]).is_ok()
        && value.get_property("icons").is_none_or(icons_valid)
        && value.get_property("_meta").is_none_or(is_object)
}

fn resource_contents_valid(value: JsonValueView<'_>) -> bool {
    is_object(value)
        && value
            .get_property("uri")
            .is_some_and(|uri| uri.as_str().is_some())
        && optional_types(value, &["mimeType"], &[], &[]).is_ok()
        && value.get_property("_meta").is_none_or(is_object)
        && (value
            .get_property("text")
            .is_some_and(|text| text.as_str().is_some())
            || value
                .get_property("blob")
                .and_then(|blob| blob.as_str())
                .is_some_and(base64_valid))
}

fn resource_entry_valid(value: JsonValueView<'_>, template: bool) -> bool {
    common_entry_valid(value)
        && value
            .get_property(if template { "uriTemplate" } else { "uri" })
            .is_some_and(|uri| uri.as_str().is_some())
        && optional_types(value, &["mimeType"], &["size"], &[]).is_ok()
        && value
            .get_property("annotations")
            .is_none_or(annotations_valid)
}

fn content_valid(value: JsonValueView<'_>) -> bool {
    if !is_object(value)
        || value
            .get_property("annotations")
            .is_some_and(|v| !annotations_valid(v))
        || value.get_property("_meta").is_some_and(|v| !is_object(v))
    {
        return false;
    }
    let Some(kind) = value.get_property("type") else {
        return false;
    };
    if is_one_of(kind, &["text"]) {
        return value
            .get_property("text")
            .is_some_and(|v| v.as_str().is_some());
    }
    if is_one_of(kind, &["image", "audio"]) {
        return value
            .get_property("data")
            .and_then(|data| data.as_str())
            .is_some_and(base64_valid)
            && value
                .get_property("mimeType")
                .is_some_and(|v| v.as_str().is_some());
    }
    if is_one_of(kind, &["resource_link"]) {
        return resource_entry_valid(value, false);
    }
    if is_one_of(kind, &["resource"]) {
        return value
            .get_property("resource")
            .is_some_and(resource_contents_valid);
    }
    false
}

fn sampling_content_valid(value: JsonValueView<'_>) -> bool {
    if !is_object(value)
        || value
            .get_property("_meta")
            .is_some_and(|meta| !is_object(meta))
    {
        return false;
    }
    let Some(kind) = value.get_property("type") else {
        return false;
    };
    if is_one_of(kind, &["text", "image", "audio"]) {
        return content_valid(value);
    }
    if is_one_of(kind, &["tool_use"]) {
        return value
            .get_property("id")
            .is_some_and(|id| id.as_str().is_some())
            && value
                .get_property("name")
                .is_some_and(|name| name.as_str().is_some())
            && value.get_property("input").is_some_and(is_object);
    }
    if is_one_of(kind, &["tool_result"]) {
        return value
            .get_property("toolUseId")
            .is_some_and(|id| id.as_str().is_some())
            && value
                .get_property("content")
                .is_some_and(|content| array_all(content, content_valid))
            && optional_type(value, "isError", "boolean").is_ok();
    }
    false
}

fn tool_entry_valid(value: JsonValueView<'_>) -> bool {
    if !common_entry_valid(value) {
        return false;
    }
    let Some(input) = value.get_property("inputSchema") else {
        return false;
    };
    is_object(input)
        && input
            .get_property("type")
            .is_some_and(|kind| is_one_of(kind, &["object"]))
        && optional_type(input, "$schema", "string").is_ok()
        && value.get_property("outputSchema").is_none_or(|schema| {
            is_object(schema) && optional_type(schema, "$schema", "string").is_ok()
        })
        && value.get_property("annotations").is_none_or(|annotations| {
            is_object(annotations)
                && optional_types(
                    annotations,
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

fn prompt_entry_valid(value: JsonValueView<'_>) -> bool {
    common_entry_valid(value)
        && value.get_property("arguments").is_none_or(|arguments| {
            array_all(arguments, |argument| {
                argument
                    .get_property("name")
                    .is_some_and(|name| name.as_str().is_some())
                    && optional_types(argument, &["description"], &[], &["required"]).is_ok()
            })
        })
}

fn error_data(method: &str, violation: &str) -> Utf16JsonProjection {
    source_object(vec![
        ("method".to_owned(), string_value(method)),
        ("violation".to_owned(), string_value(violation)),
    ])
}

fn method_error_data(method: &str) -> Utf16JsonProjection {
    source_object(vec![("method".to_owned(), string_value(method))])
}

fn result_type_error_data(method: &str, result_type: Utf16JsonProjection) -> Utf16JsonProjection {
    source_object(vec![
        ("resultType".to_owned(), result_type),
        ("method".to_owned(), string_value(method)),
    ])
}

fn invalid(method: &str, reason: &str, violation: &str) -> McpSdkError {
    McpSdkError::new(
        "INVALID_RESULT",
        format!("Invalid result for {method}: {reason}"),
        Some(error_data(method, violation)),
    )
}

fn project_fields(value: &Utf16JsonProjection, fields: &[&str]) -> Utf16JsonProjection {
    let mut selected = Vec::new();
    for field in fields {
        if value.contains_property(field) {
            selected.push((
                (*field).to_owned(),
                value
                    .subprojection(&format!("/{field}"))
                    .expect("present fixed field has a subtree"),
            ));
        }
    }
    source_object(selected)
}

fn project_meta(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let Some(entries) = value.object_entries() else {
        return value.clone();
    };
    let mut projected = Vec::new();
    for entry in entries {
        let subtree = value
            .subprojection_property_units(entry.code_units)
            .expect("entry exists");
        if entry.display_key == "io.modelcontextprotocol/serverInfo" {
            let Some(info) = subtree.at_pointer("") else {
                continue;
            };
            if !info
                .get_property("name")
                .is_some_and(|name| name.as_str().is_some())
                || !info
                    .get_property("version")
                    .is_some_and(|version| version.as_str().is_some())
                || optional_types(info, &["title", "websiteUrl", "description"], &[], &[]).is_err()
                || info
                    .get_property("icons")
                    .is_some_and(|icons| !icons_valid(icons))
            {
                continue;
            }
            let mut info = project_fields(
                &subtree,
                &[
                    "name",
                    "title",
                    "icons",
                    "version",
                    "websiteUrl",
                    "description",
                ],
            );
            if let Some(icons) = subtree.get_property("icons") {
                if icons_valid(icons) {
                    info.set_field(
                        "icons",
                        project_icons(&subtree.subprojection("/icons").expect("present")),
                    )
                    .expect("serverInfo is an object");
                }
            }
            projected.push((entry.code_units.to_vec(), info));
        } else {
            projected.push((entry.code_units.to_vec(), subtree));
        }
    }
    Utf16JsonProjection::object_units(projected).expect("source metadata keys are unique")
}

fn project_icons(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let Some(length) = value.array_len("") else {
        return value.clone();
    };
    let icons = (0..length)
        .map(|index| {
            let pointer = format!("/{index}");
            let icon = value.subprojection(&pointer).expect("array item exists");
            project_fields(&icon, &["src", "mimeType", "sizes", "theme"])
        })
        .collect();
    Utf16JsonProjection::array(icons).expect("projected icon array is valid")
}

fn project_public_result(value: Utf16JsonProjection) -> Utf16JsonProjection {
    let Some(entries) = value.object_entries() else {
        return value;
    };
    let mut output = Vec::new();
    if let Some(meta) = value.get_property("_meta") {
        let projected = if is_object(meta) {
            project_meta(&value.subprojection("/_meta").expect("meta exists"))
        } else {
            value.subprojection("/_meta").expect("meta exists")
        };
        output.push(("_meta".encode_utf16().collect(), projected));
    }
    for entry in entries {
        if entry.display_key == "_meta" || entry.display_key == "resultType" {
            continue;
        }
        output.push((
            entry.code_units.to_vec(),
            value
                .subprojection_property_units(entry.code_units)
                .expect("entry exists"),
        ));
    }
    Utf16JsonProjection::object_units(output).expect("public result keys are unique")
}

fn validate_complete(method: &str, value: &Utf16JsonProjection) -> Result<(), String> {
    let root = value.at_pointer("").ok_or("result is missing")?;
    if !is_object(root) {
        return Err("not an object".into());
    }
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
    optional_type(root, "_meta", "object")?;
    if matches!(
        method,
        "tools/list"
            | "prompts/list"
            | "resources/list"
            | "resources/templates/list"
            | "resources/read"
    ) {
        let ttl = required(root, "ttlMs", "number", &[])?;
        if !is_integer(ttl) || ttl.as_number().is_none_or(|number| number < 0.0) {
            return Err("ttlMs must be a non-negative integer".into());
        }
        let scope = required(root, "cacheScope", "string", &[])?;
        if !is_one_of(scope, &["public", "private"]) {
            return Err("Invalid cacheScope".into());
        }
    }
    match method {
        "tools/call" => {
            let content = required(root, "content", "array", &[])?;
            if !array_all(content, content_valid) {
                return Err("invalid content entry".into());
            }
            optional_type(root, "isError", "boolean")?;
        }
        "tools/list" => {
            let tools = required(root, "tools", "array", &[])?;
            if !array_all(tools, tool_entry_valid) {
                return Err("invalid tool entry".into());
            }
            optional_type(root, "nextCursor", "string")?;
        }
        "prompts/list" | "resources/list" | "resources/templates/list" | "resources/read" => {
            let field = match method {
                "prompts/list" => "prompts",
                "resources/list" => "resources",
                "resources/templates/list" => "resourceTemplates",
                _ => "contents",
            };
            let items = required(root, field, "array", &[])?;
            let valid = match method {
                "prompts/list" => array_all(items, prompt_entry_valid),
                "resources/list" => array_all(items, |item| resource_entry_valid(item, false)),
                "resources/templates/list" => {
                    array_all(items, |item| resource_entry_valid(item, true))
                }
                _ => array_all(items, resource_contents_valid),
            };
            if !valid {
                return Err(format!("Invalid {field} entry"));
            }
            optional_type(root, "nextCursor", "string")?;
        }
        "prompts/get" => {
            let messages = required(root, "messages", "array", &[])?;
            if !array_all(messages, |message| {
                message
                    .get_property("role")
                    .is_some_and(|role| is_one_of(role, &["user", "assistant"]))
                    && message.get_property("content").is_some_and(content_valid)
            }) {
                return Err("Invalid messages entry".into());
            }
            optional_type(root, "description", "string")?;
        }
        "completion/complete" => {
            let completion = required(root, "completion", "object", &[])?;
            let values = required(completion, "values", "array", &["completion"])?;
            if values.array_len().is_none_or(|length| length > 100) || !string_array(values) {
                return Err("invalid completion values".into());
            }
            optional_type(completion, "hasMore", "boolean")?;
            optional_type(completion, "total", "number")?;
            if completion
                .get_property("total")
                .is_some_and(|total| !is_integer(total))
            {
                return Err("Invalid completion total".into());
            }
        }
        "server/discover" => {
            let versions = required(root, "supportedVersions", "array", &[])?;
            if !string_array(versions) {
                return Err("invalid server discovery result".into());
            }
            required(root, "capabilities", "object", &[])?;
            optional_type(root, "instructions", "string")?;
        }
        _ => {}
    }
    Ok(())
}

fn object_entries_in_js_order(value: &Utf16JsonProjection) -> Vec<(Vec<u16>, Utf16JsonProjection)> {
    let Some(entries) = value.object_entries() else {
        return Vec::new();
    };
    let mut indexed = Vec::new();
    let mut rest = Vec::new();
    for entry in entries {
        let units = entry.code_units.to_vec();
        let subtree = value
            .subprojection_property_units(entry.code_units)
            .expect("entry exists");
        let numeric_index = String::from_utf16(&units).ok().and_then(|key| {
            key.parse::<u32>()
                .ok()
                .filter(|number| *number != u32::MAX && number.to_string() == key)
        });
        if let Some(number) = numeric_index {
            indexed.push((number, units, subtree));
        } else {
            rest.push((units, subtree));
        }
    }
    indexed.sort_unstable_by_key(|(number, _, _)| *number);
    indexed
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(rest)
        .collect()
}

/// Return exact JavaScript property names in `Object.keys` order.
pub fn javascript_object_keys(value: &Utf16JsonProjection) -> Vec<Vec<u16>> {
    object_entries_in_js_order(value)
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

fn input_requests(root: &Utf16JsonProjection) -> Vec<SourceInputRequest> {
    let Ok(requests) = root.subprojection("/inputRequests") else {
        return Vec::new();
    };
    object_entries_in_js_order(&requests)
        .into_iter()
        .map(|(key_units, request)| SourceInputRequest { key_units, request })
        .collect()
}

pub fn decode_modern_result(
    method: &str,
    value: Utf16JsonProjection,
) -> Result<DecodedMcpResult, McpSdkError> {
    let value = normalize_mcp_safe_integral_doubles(value);
    let Some(root) = value.at_pointer("") else {
        return Err(McpSdkError::new(
            "INVALID_RESULT",
            format!("Invalid result for {method}: not an object"),
            Some(method_error_data(method)),
        ));
    };
    if !is_object(root) {
        return Err(McpSdkError::new(
            "INVALID_RESULT",
            format!("Invalid result for {method}: not an object"),
            Some(method_error_data(method)),
        ));
    }
    let Some(tag_projection) = root
        .get_property("resultType")
        .and_then(|_| value.subprojection("/resultType").ok())
    else {
        return Err(invalid(
            method,
            "missing required resultType — servers implementing protocol revision 2026-07-28 MUST include it (the absent-means-complete bridge applies only to earlier-revision servers)",
            "missing-resultType",
        ));
    };
    let Some(tag) = tag_projection.string_at("") else {
        return Err(McpSdkError::new(
            "INVALID_RESULT",
            format!("Invalid result for {method}: non-string resultType"),
            Some(result_type_error_data(method, tag_projection.clone())),
        ));
    };
    match tag {
        "complete" => {
            validate_complete(method, &value).map_err(|reason| {
                McpSdkError::new(
                    "INVALID_RESULT",
                    format!("Invalid result for {method}: {reason}"),
                    Some(method_error_data(method)),
                )
            })?;
            let mut complete = value;
            complete
                .remove_field("resultType")
                .expect("decoded complete result is an object");
            Ok(DecodedMcpResult::Complete(complete))
        }
        "input_required" => {
            let requests = input_requests(&value);
            let state = value
                .string_at("/requestState")
                .map(|_| value.subprojection("/requestState").expect("present"));
            if requests.is_empty() && state.is_none() {
                return Err(invalid(
                    method,
                    "input_required carries neither inputRequests nor requestState (every input_required result must include at least one of the two)",
                    "input-required-missing-both",
                ));
            }
            Ok(DecodedMcpResult::InputRequired {
                input_requests: requests,
                request_state: state,
            })
        }
        "task" => Ok(DecodedMcpResult::Task(value)),
        other => Err(McpSdkError::new(
            "UNSUPPORTED_RESULT_TYPE",
            format!("Unsupported result type '{other}' for {method}"),
            Some(result_type_error_data(method, tag_projection)),
        )),
    }
}

pub fn rebuild_input_params(
    original: &Utf16JsonProjection,
    responses: Utf16JsonProjection,
    state: Option<&Utf16JsonProjection>,
) -> Utf16JsonProjection {
    let mut params = if original.object_entries().is_some() {
        original.clone()
    } else {
        Utf16JsonProjection::object(Vec::new()).expect("empty object")
    };
    if responses
        .object_entries()
        .is_some_and(|entries| !entries.is_empty())
    {
        params
            .set_field("inputResponses", responses)
            .expect("input-required params are an object");
    }
    if let Some(state) = state {
        params
            .set_field("requestState", state.clone())
            .expect("input-required params are an object");
    }
    params
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpResultProgress {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<Utf16JsonProjection>,
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
        params: Utf16JsonProjection,
        options: &McpInputRequiredOptions,
    ) -> Result<Utf16JsonProjection, McpResultError>;

    async fn dispatch_local(
        &self,
        key_units: Vec<u16>,
        request: Utf16JsonProjection,
        cancellation: CancellationToken,
    ) -> Result<Utf16JsonProjection, McpResultError>;
}

fn generated_error(code: &'static str, message: impl Into<String>) -> McpResultError {
    McpSdkError::new(code, message, None).into()
}

fn cancelled() -> McpResultError {
    generated_error("REQUEST_TIMEOUT", "Request cancelled")
}

fn total_timeout(max: Duration, elapsed: Duration) -> McpResultError {
    let data = source_object(vec![
        (
            "maxTotalTimeout".to_owned(),
            number_value(max.as_millis() as f64),
        ),
        (
            "totalElapsed".to_owned(),
            number_value(elapsed.as_millis() as f64),
        ),
    ]);
    McpSdkError::new(
        "REQUEST_TIMEOUT",
        "Maximum total timeout exceeded",
        Some(data),
    )
    .into()
}

pub async fn drive_modern_request(
    io: &dyn McpInputRequiredIo,
    method: &str,
    original: Utf16JsonProjection,
    options: McpInputRequiredOptions,
) -> Result<Utf16JsonProjection, McpResultError> {
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
                    Some(method_error_data(method)),
                )
                .into());
            }
            DecodedMcpResult::InputRequired {
                input_requests,
                request_state,
            } => (input_requests, request_state),
        };
        if !options.auto_fulfill {
            return Err(McpSdkError::new(
                "UNSUPPORTED_RESULT_TYPE",
                format!("Unsupported result type 'input_required' for {method}: multi-round-trip auto-fulfilment is not enabled on this instance — pass allowInputRequired: true to handle it manually, or enable inputRequired.autoFulfill"),
                Some(result_type_error_data(method, string_value("input_required"))),
            )
            .into());
        }
        if round > options.max_rounds {
            let request_projection = Utf16JsonProjection::object_units(
                requests
                    .iter()
                    .map(|request| (request.key_units.clone(), request.request.clone()))
                    .collect(),
            )
            .expect("input request keys are unique");
            let mut last_result =
                source_object(vec![("inputRequests".to_owned(), request_projection)]);
            if let Some(state) = &state {
                last_result
                    .set_field("requestState", state.clone())
                    .expect("last result is an object");
            }
            let data = source_object(vec![
                ("rounds".to_owned(), number_value(options.max_rounds as f64)),
                ("lastResult".to_owned(), last_result),
            ]);
            return Err(McpSdkError::new(
                "INPUT_REQUIRED_ROUNDS_EXCEEDED",
                format!(
                    "Multi-round-trip request '{method}' still required input after {} rounds (inputRequired.maxRounds)",
                    options.max_rounds
                ),
                Some(data),
            )
            .into());
        }
        if let Some(progress) = &options.on_progress {
            progress(McpResultProgress {
                progress: round as f64,
                total: None,
                message: Some(string_value(&format!(
                    "Fulfilling input required by '{method}' (round {round})"
                ))),
            });
        }
        let mut completed = Vec::with_capacity(requests.len());
        if requests.is_empty() {
            tokio::select! {
                _ = options.cancellation.cancelled() => return Err(cancelled()),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
        } else {
            let scope = options.cancellation.child_token();
            let _scope_guard = scope.clone().drop_guard();
            let mut pending = FuturesUnordered::new();
            for (order, request) in requests.into_iter().enumerate() {
                let cancellation = scope.clone();
                pending.push(async move {
                    let key_units = request.key_units;
                    let result = io
                        .dispatch_local(key_units.clone(), request.request, cancellation)
                        .await;
                    tokio::task::yield_now().await;
                    (order, key_units, result)
                });
            }
            loop {
                let next = tokio::select! {
                    biased;
                    _ = scope.cancelled() => return Err(cancelled()),
                    next = pending.next() => next,
                };
                let Some((order, key, outcome)) = next else {
                    break;
                };
                match outcome {
                    Ok(value) => completed.push((order, key, value)),
                    Err(error) => {
                        scope.cancel();
                        return Err(error);
                    }
                }
            }
            drop(pending);
        }
        completed.sort_by_key(|(order, _, _)| *order);
        let responses = Utf16JsonProjection::object_units(
            completed
                .into_iter()
                .map(|(_, key, value)| (key, value))
                .collect(),
        )
        .expect("input response keys are unique");
        if let Some(max) = options.max_total_timeout {
            let elapsed = started.elapsed();
            if elapsed >= max {
                return Err(total_timeout(max, elapsed));
            }
            wire_options.max_total_timeout = Some(max - elapsed);
        }
        let params = rebuild_input_params(&original, responses, state.as_ref());
        result = io.request(method, params, &wire_options).await?;
    }
    unreachable!("bounded input-required loop")
}

pub struct JsonrpcMcpResultIo {
    pub connection: Arc<jsonrpc::Connection>,
    pub client_capabilities: Utf16JsonProjection,
}

struct CancelRequest {
    connection: Arc<jsonrpc::Connection>,
    id: jsonrpc::Id,
    armed: bool,
    reason: String,
    meta: Option<Utf16JsonProjection>,
}

impl Drop for CancelRequest {
    fn drop(&mut self) {
        if self.armed && !self.connection.has_per_request_cancellation() {
            let mut params = Utf16JsonProjection::object(vec![
                ("requestId".to_owned(), id_projection(&self.id)),
                ("reason".to_owned(), string_value(&self.reason)),
            ])
            .expect("fixed cancellation fields are unique");
            if let Some(meta) = &self.meta {
                params
                    .set_field("_meta", meta.clone())
                    .expect("cancellation params are an object");
            }
            let raw = params
                .to_json_string()
                .expect("cancellation projection is serializable");
            if let Ok(payload) = jsonrpc::JsonPayload::parse(raw) {
                let _ = self.connection.notify("notifications/cancelled", payload);
            }
        }
    }
}

fn id_projection(id: &jsonrpc::Id) -> Utf16JsonProjection {
    match id {
        jsonrpc::Id::Number(number) => Utf16JsonProjection::parse(&number.to_string())
            .expect("generated request id is valid JSON"),
        jsonrpc::Id::String(value) => string_value(value),
        jsonrpc::Id::StringUnits(units) => {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.clone())
                .expect("source request id has valid UTF-16 units")
        }
    }
}

fn string_id(units: &[u16]) -> jsonrpc::Id {
    match String::from_utf16(units) {
        Ok(value) => jsonrpc::Id::String(value),
        Err(_) => jsonrpc::Id::StringUnits(units.to_vec()),
    }
}

fn source_id_matches(value: JsonValueView<'_>, id: &jsonrpc::Id) -> bool {
    match id {
        jsonrpc::Id::Number(number) => value.as_number() == Some(*number as f64),
        jsonrpc::Id::String(text) => value.as_str() == Some(text),
        jsonrpc::Id::StringUnits(units) => value.code_units() == Some(units.as_slice()),
    }
}

fn jsonrpc_method_is(method: &jsonrpc::JsonRpcString, expected: &str) -> bool {
    method
        .code_units()
        .iter()
        .copied()
        .eq(expected.encode_utf16())
}

fn protocol_meta_projection(params: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let meta = params.subprojection("/_meta").ok()?;
    let entries = meta.object_entries()?;
    let fields = entries
        .into_iter()
        .filter(|entry| {
            entry.code_units.starts_with(
                &"io.modelcontextprotocol/"
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            )
        })
        .map(|entry| {
            (
                entry.code_units.to_vec(),
                meta.subprojection_property_units(entry.code_units)
                    .expect("entry exists"),
            )
        })
        .collect();
    Utf16JsonProjection::object_units(fields).ok()
}

fn protocol_invalid(message: String) -> McpResultError {
    McpResultError::Local(jsonrpc::ResponseError {
        code: jsonrpc::INVALID_PARAMS,
        message: message.into(),
        data: None,
    })
}

fn embedded_method<'a>(
    key: &[u16],
    request: &'a Utf16JsonProjection,
) -> Result<&'a str, McpResultError> {
    let method = request.string_at("/method").ok_or_else(|| {
        McpResultError::Sdk(McpSdkError::new(
            "INVALID_RESULT",
            format!(
                "Invalid input request '{}': each inputRequests entry must be an embedded request object with a method",
                String::from_utf16_lossy(key)
            ),
            Some(source_object(vec![("key".into(), string_units_value(key))])),
        ))
    })?;
    if !matches!(
        method,
        "roots/list" | "elicitation/create" | "sampling/createMessage"
    ) {
        return Err(McpSdkError::new(
            "INVALID_RESULT",
            format!(
                "Invalid input request '{}': '{method}' is not an embedded request the 2026-07-28 revision defines (expected elicitation/create, sampling/createMessage, or roots/list)",
                String::from_utf16_lossy(key)
            ),
            Some(source_object(vec![
                ("key".into(), string_units_value(key)),
                ("method".into(), string_value(method)),
            ])),
        )
        .into());
    }
    Ok(method)
}

fn project_embedded_request(
    method: &str,
    request: &Utf16JsonProjection,
    key_units: &[u16],
) -> Result<jsonrpc::Request, McpResultError> {
    let params = request
        .at_pointer("")
        .and_then(|root| root.get_property("params"))
        .filter(|params| is_object(*params))
        .and_then(|_| request.subprojection("/params").ok())
        .map(normalize_mcp_safe_integral_doubles);
    let projected = match (method, params) {
        ("roots/list", Some(params)) => Some(params),
        ("roots/list", None) => None,
        ("elicitation/create", Some(params)) => Some(project_embedded_params(method, &params)),
        ("sampling/createMessage", Some(params)) => Some(project_embedded_params(method, &params)),
        ("sampling/createMessage", None) => {
            return Err(protocol_invalid(
                "Invalid sampling request: params must be an object".into(),
            ))
        }
        (_, Some(params)) => Some(params),
        (_, None) => None,
    };
    let params = projected
        .map(|projected| {
            let raw = projected.to_json_string().map_err(|error| {
                McpResultError::Sdk(McpSdkError::new("INVALID_RESULT", error.to_string(), None))
            })?;
            jsonrpc::JsonPayload::parse(raw).map_err(|error| {
                McpResultError::Sdk(McpSdkError::new("INVALID_RESULT", error.to_string(), None))
            })
        })
        .transpose()?;
    Ok(jsonrpc::Request::new(method, params, string_id(key_units)))
}

fn project_array(
    value: &Utf16JsonProjection,
    mut projector: impl FnMut(&Utf16JsonProjection) -> Utf16JsonProjection,
) -> Utf16JsonProjection {
    let Some(length) = value.array_len("") else {
        return value.clone();
    };
    let items = (0..length)
        .map(|index| {
            let item = value
                .subprojection(&format!("/{index}"))
                .expect("array item exists");
            projector(&item)
        })
        .collect();
    Utf16JsonProjection::array(items).expect("projected array is valid")
}

fn project_source_content(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let kind = value.string_at("/type").unwrap_or("");
    let fields: &[&str] = match kind {
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
        _ => return value.clone(),
    };
    let mut result = project_fields(value, fields);
    if value.contains_property("annotations") {
        let annotations = value.subprojection("/annotations").expect("present");
        result
            .set_field(
                "annotations",
                project_fields(&annotations, &["audience", "priority", "lastModified"]),
            )
            .expect("content projection is an object");
    }
    if value.contains_property("icons") {
        let icons = value.subprojection("/icons").expect("present");
        result
            .set_field(
                "icons",
                project_array(&icons, |icon| {
                    project_fields(icon, &["src", "mimeType", "sizes", "theme"])
                }),
            )
            .expect("content projection is an object");
    }
    if kind == "resource" {
        if let Ok(resource) = value.subprojection("/resource") {
            let projected = if resource.contains_property("text") {
                project_fields(&resource, &["uri", "mimeType", "_meta", "text"])
            } else {
                project_fields(&resource, &["uri", "mimeType", "_meta", "blob"])
            };
            result
                .set_field("resource", projected)
                .expect("content projection is an object");
        }
    }
    if kind == "tool_result" {
        if let Ok(content) = value.subprojection("/content") {
            result
                .set_field("content", project_array(&content, project_source_content))
                .expect("content projection is an object");
        }
    }
    result
}

fn project_source_message(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut result = project_fields(value, &["role", "content", "_meta"]);
    if let Ok(content) = value.subprojection("/content") {
        let projected = if content.array_len("").is_some() {
            project_array(&content, project_source_content)
        } else {
            project_source_content(&content)
        };
        result
            .set_field("content", projected)
            .expect("message projection is an object");
    }
    result
}

fn project_source_tool(value: &Utf16JsonProjection) -> Utf16JsonProjection {
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
    if let Ok(annotations) = value.subprojection("/annotations") {
        result
            .set_field(
                "annotations",
                project_fields(
                    &annotations,
                    &[
                        "title",
                        "readOnlyHint",
                        "destructiveHint",
                        "idempotentHint",
                        "openWorldHint",
                    ],
                ),
            )
            .expect("tool projection is an object");
    }
    if let Ok(icons) = value.subprojection("/icons") {
        result
            .set_field(
                "icons",
                project_array(&icons, |icon| {
                    project_fields(icon, &["src", "mimeType", "sizes", "theme"])
                }),
            )
            .expect("tool projection is an object");
    }
    result
}

fn project_source_elicitation_property(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let kind = value.string_at("/type").unwrap_or("");
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
        "string"
            if value
                .subprojection("/enum")
                .is_ok_and(|items| string_array(items.at_pointer("").expect("root"))) =>
        {
            if value.subprojection("/enumNames").map_or(true, |items| {
                string_array(items.at_pointer("").expect("root"))
            }) {
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
            if value.subprojection("/oneOf").is_ok_and(|choices| {
                choices.array_len("").is_some_and(|length| {
                    (0..length).all(|index| {
                        let choice = choices.subprojection(&format!("/{index}")).ok();
                        choice.is_some_and(|choice| {
                            choice.string_at("/const").is_some()
                                && choice.string_at("/title").is_some()
                        })
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
        _ => return value.clone(),
    };
    let mut result = project_fields(value, fields);
    if fields.contains(&"oneOf") {
        if let Ok(choices) = value.subprojection("/oneOf") {
            result
                .set_field(
                    "oneOf",
                    project_array(&choices, |choice| {
                        project_fields(choice, &["const", "title"])
                    }),
                )
                .expect("property projection is an object");
        }
    }
    if kind == "array" {
        if let Ok(items) = value.subprojection("/items") {
            let projected = if items.string_at("/type") == Some("string")
                && items.subprojection("/enum").is_ok()
            {
                project_fields(&items, &["type", "enum"])
            } else if let Ok(any_of) = items.subprojection("/anyOf") {
                let mut projected = source_object(vec![]);
                projected
                    .set_field(
                        "anyOf",
                        project_array(&any_of, |choice| {
                            project_fields(choice, &["const", "title"])
                        }),
                    )
                    .expect("items projection is an object");
                projected
            } else {
                items
            };
            result
                .set_field("items", projected)
                .expect("property projection is an object");
        }
    }
    result
}

fn project_source_requested_schema(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut fields = project_fields(value, &["type", "properties", "required"]);
    if let Ok(properties) = value.subprojection("/properties") {
        let entries = properties.object_entries().unwrap_or_default();
        let projected = Utf16JsonProjection::object_units(
            entries
                .into_iter()
                .map(|entry| {
                    (
                        entry.code_units.to_vec(),
                        project_source_elicitation_property(
                            &properties
                                .subprojection_property_units(entry.code_units)
                                .expect("property exists"),
                        ),
                    )
                })
                .collect(),
        )
        .expect("schema property keys are unique");
        fields
            .set_field("properties", projected)
            .expect("requested schema is an object");
    }
    let known = ["type", "properties", "required"];
    if let Some(entries) = value.object_entries() {
        for entry in entries {
            if known.contains(&entry.display_key) {
                continue;
            }
            if let Ok(extra) = value.subprojection_property_units(entry.code_units) {
                let extra =
                    Utf16JsonProjection::object_units(vec![(entry.code_units.to_vec(), extra)])
                        .expect("single exact schema key is unique");
                fields
                    .merge_object_projection(&extra)
                    .expect("requested schema is an object");
            }
        }
    }
    fields
}

fn project_embedded_params(method: &str, value: &Utf16JsonProjection) -> Utf16JsonProjection {
    match method {
        "roots/list" => value.clone(),
        "elicitation/create" if value.string_at("/mode") == Some("url") => {
            project_fields(value, &["mode", "message", "url"])
        }
        "elicitation/create" => {
            let mut result = project_fields(
                value,
                &["_meta", "task", "mode", "message", "requestedSchema"],
            );
            if let Ok(task) = value.subprojection("/task") {
                result
                    .set_field("task", project_fields(&task, &["ttl"]))
                    .expect("elicitation params are an object");
            }
            if let Ok(schema) = value.subprojection("/requestedSchema") {
                result
                    .set_field("requestedSchema", project_source_requested_schema(&schema))
                    .expect("elicitation params are an object");
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
            if let Ok(messages) = value.subprojection("/messages") {
                result
                    .set_field("messages", project_array(&messages, project_source_message))
                    .expect("sampling params are an object");
            }
            if let Ok(preferences) = value.subprojection("/modelPreferences") {
                let mut projected = project_fields(
                    &preferences,
                    &[
                        "hints",
                        "costPriority",
                        "speedPriority",
                        "intelligencePriority",
                    ],
                );
                if let Ok(hints) = preferences.subprojection("/hints") {
                    projected
                        .set_field(
                            "hints",
                            project_array(&hints, |hint| project_fields(hint, &["name"])),
                        )
                        .expect("model preferences are an object");
                }
                result
                    .set_field("modelPreferences", projected)
                    .expect("sampling params are an object");
            }
            if let Ok(tools) = value.subprojection("/tools") {
                result
                    .set_field("tools", project_array(&tools, project_source_tool))
                    .expect("sampling params are an object");
            }
            if let Ok(choice) = value.subprojection("/toolChoice") {
                result
                    .set_field("toolChoice", project_fields(&choice, &["mode"]))
                    .expect("sampling params are an object");
            }
            result
        }
        _ => value.clone(),
    }
}

fn validate_local_result(method: &str, result: &Utf16JsonProjection) -> Result<(), McpResultError> {
    let root = result
        .at_pointer("")
        .ok_or_else(|| protocol_invalid("Invalid local result".into()))?;
    let valid = match method {
        "roots/list" => true,
        "elicitation/create" => {
            root.get_property("action")
                .is_some_and(|action| is_one_of(action, &["accept", "decline", "cancel"]))
                && root
                    .get_property("content")
                    .is_none_or(elicitation_content_valid)
        }
        "sampling/createMessage" => {
            root.get_property("model")
                .is_some_and(|model| model.as_str().is_some())
                && root
                    .get_property("role")
                    .is_some_and(|role| is_one_of(role, &["user", "assistant"]))
                && root.get_property("content").is_some_and(|content| {
                    sampling_content_valid(content) || array_all(content, sampling_content_valid)
                })
                && root.get_property("_meta").is_none_or(is_object)
                && root
                    .get_property("stopReason")
                    .is_none_or(|reason| reason.as_str().is_some())
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(protocol_invalid(format!("Invalid {method} result")))
    }
}

fn elicitation_content_valid(content: JsonValueView<'_>) -> bool {
    let Some(entries) = content.object_entries() else {
        return false;
    };
    entries.into_iter().all(|entry| {
        let value = entry.value;
        value.as_str().is_some()
            || value.as_number().is_some()
            || value.as_bool().is_some()
            || string_array(value)
    })
}

fn elicitation_property_valid(property: JsonValueView<'_>) -> bool {
    if !is_object(property)
        || optional_types(property, &["title", "description"], &[], &[]).is_err()
    {
        return false;
    }
    let Some(kind) = property.get_property("type") else {
        return false;
    };
    if is_one_of(kind, &["boolean"]) {
        return property
            .get_property("default")
            .is_none_or(|value| value.as_bool().is_some());
    }
    if is_one_of(kind, &["number", "integer"]) {
        return optional_types(property, &[], &["minimum", "maximum", "default"], &[]).is_ok();
    }
    if is_one_of(kind, &["string"]) {
        let plain = optional_types(property, &[], &["minLength", "maxLength"], &[]).is_ok()
            && property
                .get_property("format")
                .is_none_or(|format| is_one_of(format, &["email", "uri", "date", "date-time"]));
        let enumeration = property.get_property("enum").is_some_and(string_array);
        let choices = property.get_property("oneOf").is_some_and(|choices| {
            array_all(choices, |choice| {
                choice
                    .get_property("const")
                    .is_some_and(|value| value.as_str().is_some())
                    && choice
                        .get_property("title")
                        .is_some_and(|value| value.as_str().is_some())
            })
        });
        return property
            .get_property("default")
            .is_none_or(|value| value.as_str().is_some())
            && (plain || enumeration || choices);
    }
    if is_one_of(kind, &["array"]) {
        return optional_types(property, &[], &["minItems", "maxItems"], &[]).is_ok()
            && property.get_property("default").is_none_or(string_array)
            && property.get_property("items").is_some_and(|items| {
                is_object(items)
                    && ((items
                        .get_property("type")
                        .is_some_and(|kind| is_one_of(kind, &["string"]))
                        && items.get_property("enum").is_some_and(string_array))
                        || items.get_property("anyOf").is_some_and(|choices| {
                            array_all(choices, |choice| {
                                choice
                                    .get_property("const")
                                    .is_some_and(|value| value.as_str().is_some())
                                    && choice
                                        .get_property("title")
                                        .is_some_and(|value| value.as_str().is_some())
                            })
                        }))
            });
    }
    false
}

fn validate_embedded_params(method: &str, params: Option<JsonValueView<'_>>) -> Result<(), String> {
    // Native 2.1.295 accepted a roots/list string params value. The roots
    // projection below omits non-object params before local dispatch, so keep
    // that method-specific behavior without relaxing other embedded methods.
    if method == "roots/list" {
        return Ok(());
    }
    let Some(params) = params else {
        return Err("params must be an object".into());
    };
    if !is_object(params) {
        return Err("params must be an object".into());
    }
    match method {
        "sampling/createMessage" => {
            let messages = required(params, "messages", "array", &["params"])?;
            let max_tokens = required(params, "maxTokens", "number", &["params"])?;
            if !is_integer(max_tokens) {
                return Err("maxTokens must be an integer".into());
            }
            if !array_all(messages, |message| {
                message
                    .get_property("role")
                    .is_some_and(|role| is_one_of(role, &["user", "assistant"]))
                    && message.get_property("content").is_some_and(|content| {
                        sampling_content_valid(content)
                            || array_all(content, sampling_content_valid)
                    })
                    && message.get_property("_meta").is_none_or(is_object)
            }) {
                return Err("Invalid sampling messages".into());
            }
            optional_types(params, &["systemPrompt"], &["temperature"], &[])?;
            if params
                .get_property("includeContext")
                .is_some_and(|value| !is_one_of(value, &["none", "thisServer", "allServers"]))
                || params
                    .get_property("stopSequences")
                    .is_some_and(|value| !string_array(value))
                || params
                    .get_property("metadata")
                    .is_some_and(|value| !is_object(value))
                || params
                    .get_property("tools")
                    .is_some_and(|value| !array_all(value, tool_entry_valid))
                || params.get_property("toolChoice").is_some_and(|choice| {
                    !is_object(choice)
                        || choice
                            .get_property("mode")
                            .is_some_and(|mode| !is_one_of(mode, &["auto", "required", "none"]))
                })
            {
                return Err("Invalid sampling parameters".into());
            }
            if let Some(preferences) = params.get_property("modelPreferences") {
                if !is_object(preferences)
                    || preferences.get_property("hints").is_some_and(|hints| {
                        !array_all(hints, |hint| {
                            is_object(hint) && optional_type(hint, "name", "string").is_ok()
                        })
                    })
                    || ["costPriority", "speedPriority", "intelligencePriority"]
                        .iter()
                        .any(|key| {
                            preferences.get_property(key).is_some_and(|priority| {
                                priority
                                    .as_number()
                                    .is_none_or(|value| !(0.0..=1.0).contains(&value))
                            })
                        })
                {
                    return Err("Invalid model preferences".into());
                }
            }
            Ok(())
        }
        "elicitation/create" => {
            required(params, "message", "string", &["params"])?;
            match params.get_property("mode") {
                Some(mode) if is_one_of(mode, &["url"]) => {
                    let url = required(params, "url", "string", &["params"])?;
                    let url = url.as_str().expect("required string");
                    url::Url::parse(url)
                        .map(|_| ())
                        .map_err(|_| "Invalid URL".into())
                }
                Some(mode) if !is_one_of(mode, &["form"]) => Err("Invalid elicitation mode".into()),
                _ => {
                    let schema = required(params, "requestedSchema", "object", &["params"])?;
                    if !schema
                        .get_property("type")
                        .is_some_and(|kind| is_one_of(kind, &["object"]))
                        || !schema.get_property("properties").is_some_and(|properties| {
                            properties.object_entries().is_some_and(|entries| {
                                entries
                                    .into_iter()
                                    .all(|entry| elicitation_property_valid(entry.value))
                            })
                        })
                        || schema
                            .get_property("required")
                            .is_some_and(|required| !string_array(required))
                        || params
                            .get_property("_meta")
                            .is_some_and(|meta| !is_object(meta))
                    {
                        return Err("Invalid elicitation object schema".into());
                    }
                    Ok(())
                }
            }
        }
        _ => Ok(()),
    }
}

fn project_local_result(method: &str, result: &Utf16JsonProjection) -> Utf16JsonProjection {
    match method {
        "roots/list" => result.clone(),
        "elicitation/create" => project_fields(result, &["action", "content"]),
        "sampling/createMessage" => {
            let mut projected =
                project_fields(result, &["role", "content", "_meta", "model", "stopReason"]);
            if result.get_property("content").is_some() {
                if let Ok(content) = result.subprojection("/content") {
                    let content = if content.array_len("").is_some() {
                        project_array(&content, project_source_content)
                    } else {
                        project_source_content(&content)
                    };
                    projected
                        .set_field("content", content)
                        .expect("sampling result is an object");
                }
            }
            projected
        }
        _ => result.clone(),
    }
}

pub fn validate_embedded_request(
    key_units: &[u16],
    value: &Utf16JsonProjection,
) -> Result<jsonrpc::Request, McpResultError> {
    let method = embedded_method(key_units, value)?;
    let params = value
        .at_pointer("")
        .and_then(|root| root.get_property("params"));
    validate_embedded_params(method, params).map_err(|issue| {
        protocol_invalid(format!(
            "Invalid {} request: {issue}",
            if method == "sampling/createMessage" {
                "sampling"
            } else {
                "elicitation"
            }
        ))
    })?;
    project_embedded_request(method, value, key_units)
}

/// Validate the native embedded response union without lowering payloads.
pub fn validate_input_response_schema(value: &Utf16JsonProjection) -> bool {
    let Some(root) = value.at_pointer("") else {
        return false;
    };
    if let Some(roots) = root.get_property("roots") {
        return array_all(roots, |item| {
            item.get_property("uri")
                .and_then(|uri| uri.as_str())
                .is_some_and(|uri| uri.starts_with("file://"))
                && item
                    .get_property("name")
                    .is_none_or(|name| name.as_str().is_some())
                && item.get_property("_meta").is_none_or(is_object)
        });
    }
    root.get_property("action")
        .is_some_and(|_| validate_local_result("elicitation/create", value).is_ok())
        || root
            .get_property("model")
            .is_some_and(|_| validate_local_result("sampling/createMessage", value).is_ok())
}

pub fn validate_input_required_schema(value: &Utf16JsonProjection) -> bool {
    let Some(root) = value.at_pointer("") else {
        return false;
    };
    is_object(root)
        && root
            .get_property("resultType")
            .is_none_or(|tag| tag.as_str().is_some())
        && root.get_property("_meta").is_none_or(is_object)
        && root
            .get_property("requestState")
            .is_none_or(|state| state.as_str().is_some())
        && root.get_property("inputRequests").is_none_or(|requests| {
            requests.object_entries().is_some_and(|entries| {
                entries.into_iter().all(|entry| {
                    let Ok(request) = value.subprojection("/inputRequests").and_then(|requests| {
                        requests.subprojection_property_units(entry.code_units)
                    }) else {
                        return false;
                    };
                    validate_embedded_request(entry.code_units, &request).is_ok()
                })
            })
        })
}

pub fn validate_input_request_schema(value: &Utf16JsonProjection) -> bool {
    let Some(root) = value.at_pointer("") else {
        return false;
    };
    let params_have_schema_shape = root.get_property("params").is_none_or(|params| {
        is_object(params)
            && (value.string_at("/method") != Some("roots/list")
                || params.get_property("_meta").is_none_or(is_object))
    });
    params_have_schema_shape && validate_embedded_request(&[], value).is_ok()
}

#[async_trait]
impl McpInputRequiredIo for JsonrpcMcpResultIo {
    async fn request(
        &self,
        method: &str,
        params: Utf16JsonProjection,
        options: &McpInputRequiredOptions,
    ) -> Result<Utf16JsonProjection, McpResultError> {
        if options.cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let params = normalize_mcp_safe_integral_doubles(params);
        let mut notifications = self.connection.notifications();
        let mut progress_enabled = options.on_progress.is_some();
        let cancellation_meta = protocol_meta_projection(&params);
        let params_for_call = params.clone();
        let progress_enabled_for_call = progress_enabled;
        let call = self
            .connection
            .start_call_raw_json_with_params(method, move |id| {
                let mut params = params_for_call;
                if progress_enabled_for_call {
                    match params.get_property("_meta") {
                        Some(meta_view) if is_object(meta_view) => {
                            let mut meta = params.subprojection("/_meta").expect("meta exists");
                            meta.set_field("progressToken", id_projection(id))
                                .expect("MCP params metadata is an object");
                            params
                                .set_field("_meta", meta)
                                .expect("MCP params are an object");
                        }
                        Some(_) => {}
                        None => {
                            let meta =
                                source_object(vec![("progressToken".into(), id_projection(id))]);
                            params
                                .set_field("_meta", meta)
                                .expect("MCP params are an object");
                        }
                    }
                }
                params.to_json_string().expect("MCP params are valid JSON")
            })?;
        let id = call.id().clone();
        let mut cancellation = CancelRequest {
            connection: self.connection.clone(),
            id: id.clone(),
            armed: true,
            reason: "Request cancelled".into(),
            meta: cancellation_meta,
        };
        let wait = call.wait_raw_json();
        tokio::pin!(wait);
        let start = Instant::now();
        let mut deadline = start + options.per_request_timeout;
        loop {
            tokio::select! {
                biased;
                _ = options.cancellation.cancelled() => return Err(cancelled()),
                raw = &mut wait => {
                    cancellation.armed = false;
                    let raw = raw.map_err(|error| McpResultError::Connection(error.into()))?;
                    return Utf16JsonProjection::parse(&raw).map_err(|error| {
                        McpSdkError::new("INVALID_RESULT", error.to_string(), None).into()
                    });
                }
                _ = tokio::time::sleep_until(deadline) => {
                    cancellation.reason = "SdkError: Request timed out".into();
                    let data = source_object(vec![("timeout".into(), number_value(options.per_request_timeout.as_millis() as f64))]);
                    return Err(McpSdkError::new("REQUEST_TIMEOUT", "Request timed out", Some(data)).into());
                }
                notice = notifications.recv(), if progress_enabled => {
                    let notice = match notice {
                        Ok(notice) => notice,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => { progress_enabled = false; continue; },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    };
                    if !jsonrpc_method_is(&notice.method, "notifications/progress") { continue; }
                    let Some(params) = notice.params else { continue; };
                    let Ok(params) = Utf16JsonProjection::parse(params.as_json()) else { continue; };
                    let Some(token) = params.at_pointer("").and_then(|root| root.get_property("progressToken")) else { continue; };
                    if !source_id_matches(token, &id) { continue; }
                    if options.reset_timeout_on_progress {
                        if let Some(max) = options.max_total_timeout {
                            if start.elapsed() >= max {
                                cancellation.reason = "SdkError: Maximum total timeout exceeded".into();
                                return Err(total_timeout(max, start.elapsed()));
                            }
                        }
                        deadline = Instant::now() + options.per_request_timeout;
                    }
                    if let Some(callback) = &options.on_progress {
                        let progress = params.number_at("/progress").unwrap_or(0.0);
                        let total = params.number_at("/total");
                        let message = params
                            .subprojection("/message")
                            .ok()
                            .filter(|value| {
                                value
                                    .at_pointer("")
                                    .and_then(|value| value.code_units())
                                    .is_some()
                            });
                        callback(McpResultProgress { progress, total, message });
                    }
                }
            }
        }
    }

    async fn dispatch_local(
        &self,
        key_units: Vec<u16>,
        request_value: Utf16JsonProjection,
        cancellation: CancellationToken,
    ) -> Result<Utf16JsonProjection, McpResultError> {
        let method = embedded_method(&key_units, &request_value)?;
        let handler = self
            .connection
            .local_registered_handler(method)
            .await
            .ok_or_else(|| {
                McpResultError::Sdk(McpSdkError::new(
                    "CAPABILITY_NOT_SUPPORTED",
                    format!(
                        "Cannot fulfil input request '{}': no handler is registered for '{method}' on this client. Declare the corresponding capability and register a handler, or handle input_required results manually.",
                        String::from_utf16_lossy(&key_units)
                    ),
                    Some(source_object(vec![
                        ("key".into(), string_units_value(&key_units)),
                        ("method".into(), string_value(method)),
                    ])),
                ))
            })?;
        let request = validate_embedded_request(&key_units, &request_value)?;
        let capability = if jsonrpc_method_is(&request.method, "roots/list") {
            "roots"
        } else if jsonrpc_method_is(&request.method, "elicitation/create") {
            "elicitation"
        } else {
            "sampling"
        };
        if !self
            .client_capabilities
            .get_property(capability)
            .is_some_and(is_object)
        {
            return Err(generated_error(
                "CAPABILITY_NOT_SUPPORTED",
                format!(
                    "Client does not support {capability} capability (required for {})",
                    request.method
                ),
            ));
        }
        if jsonrpc_method_is(&request.method, "elicitation/create") {
            let mode = request
                .params
                .as_ref()
                .and_then(|params| Utf16JsonProjection::parse(params.as_json()).ok())
                .and_then(|params| params.string_at("/mode").map(str::to_owned))
                .unwrap_or_else(|| "form".to_owned());
            let caps = self.client_capabilities.get_property("elicitation");
            let supported = if mode == "url" {
                caps.and_then(|caps| caps.get_property("url"))
                    .is_some_and(is_object)
            } else {
                caps.is_some_and(|caps| {
                    caps.object_entries()
                        .is_some_and(|entries| entries.is_empty())
                        || caps.get_property("form").is_some_and(is_object)
                })
            };
            if !supported {
                return Err(protocol_invalid(format!(
                    "Client does not support {mode}-mode elicitation requests"
                )));
            }
        }
        let method = request.method.display();
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled()),
            response = handler.handle(request) => response,
        };
        if let Some(error) = response.error {
            return Err(McpResultError::Local(error));
        }
        let result = match response.result {
            Some(result) => Utf16JsonProjection::parse(result.as_json())
                .map_err(|error| McpSdkError::new("INVALID_RESULT", error.to_string(), None))?,
            None => null_value(),
        };
        let result = normalize_mcp_safe_integral_doubles(result);
        validate_local_result(&method, &result)?;
        if method == "roots/list" {
            return Ok(result);
        }
        Ok(project_local_result(&method, &result))
    }
}
