//! Source-faithful decoder for Claude Code 2.1.295's MCP JSON-RPC message
//! schema (`aX` in the frozen MCP SDK chunk).
//!
//! This is intentionally separate from the protocol-agnostic `jsonrpc`
//! message decoder. The SDK parses JSON first, then validates the resulting
//! JavaScript-shaped value against MCP's four-variant schema. Parsing through
//! `Utf16JsonProjection` preserves the observable JSON.parse behavior needed
//! here: binary64 numbers, last-value-wins duplicate keys, and exact UTF-16
//! strings.

use jsonrpc::{
    CodecError, Id, JsonPayload, JsonRpcString, Message, Notification, Request, Response,
    ResponseError,
};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::fmt;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const RELATED_TASK_META_KEY: &str = "io.modelcontextprotocol/related-task";

/// Which branch of the Native SDK's `JSONRPCMessageSchema` accepted the
/// source projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeMcpMessageKind {
    Request,
    Notification,
    ResultResponse,
    ErrorResponse,
}

/// A source-preserving JSON projection validated against the Native MCP
/// message schema. The frame adapter applies Zod's normalization to this
/// intermediate before constructing the generic JSON-RPC carrier.
#[derive(Debug, Clone)]
pub(crate) struct DecodedNativeMcpMessage {
    pub(crate) kind: NativeMcpMessageKind,
    pub(crate) projection: Utf16JsonProjection,
}

/// JSON parse or Native MCP schema error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeMcpMessageDecodeError(String);

impl fmt::Display for NativeMcpMessageDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NativeMcpMessageDecodeError {}

impl NativeMcpMessageDecodeError {
    fn invalid(reason: &'static str) -> Self {
        Self(reason.to_owned())
    }
}

/// Parse JSON with JavaScript-compatible source carriers, then validate the
/// Native MCP SDK's strict four-variant JSON-RPC message schema.
pub(crate) fn decode_native_mcp_message(
    source: &str,
) -> Result<DecodedNativeMcpMessage, NativeMcpMessageDecodeError> {
    let projection = Utf16JsonProjection::parse(source)
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
    let root = projection
        .at_pointer("")
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("message has no root value"))?;

    let kind = if has(root, "method") {
        if has(root, "id") {
            validate_request(root)?;
            NativeMcpMessageKind::Request
        } else {
            validate_notification(root)?;
            NativeMcpMessageKind::Notification
        }
    } else if has(root, "result") && !has(root, "error") {
        validate_result_response(root)?;
        NativeMcpMessageKind::ResultResponse
    } else if has(root, "error") && !has(root, "result") {
        validate_error_response(root)?;
        NativeMcpMessageKind::ErrorResponse
    } else {
        return Err(NativeMcpMessageDecodeError::invalid(
            "message does not match a Native JSON-RPC request, notification, result, or error schema",
        ));
    };

    Ok(DecodedNativeMcpMessage { kind, projection })
}

/// Decode one Native MCP JSON-RPC frame into the generic message carrier.
///
/// Validation and normalization follow the SDK's `aX.parse(JSON.parse(..))`
/// path. The source projection remains authoritative for nested payloads and
/// UTF-16 strings, including the method carrier's exact code units.
pub fn decode_native_mcp_frame(bytes: &[u8]) -> Result<Message, CodecError> {
    // Node's Buffer.toString("utf8") replaces malformed byte sequences with
    // U+FFFD before JSON.parse. Keep that ingress behavior rather than
    // rejecting the frame at Rust's stricter UTF-8 boundary.
    let source = String::from_utf8_lossy(bytes);
    let decoded = decode_native_mcp_message(&source)
        .map_err(|error| CodecError::MessageDecode(error.to_string()))?;
    let normalized = normalize_native_projection(&decoded)
        .map_err(|error| CodecError::MessageDecode(error.to_string()))?;
    message_from_normalized(decoded.kind, &normalized)
        .map_err(|error| CodecError::MessageDecode(error.to_string()))
}

fn message_from_normalized(
    kind: NativeMcpMessageKind,
    projection: &Utf16JsonProjection,
) -> Result<Message, NativeMcpMessageDecodeError> {
    let jsonrpc = "2.0".to_owned();
    match kind {
        NativeMcpMessageKind::Request => Ok(Message::Request(Request {
            jsonrpc,
            id: id_member(projection, "id")?,
            method: jsonrpc_method(projection)?,
            params: optional_payload_member(projection, "params")?,
        })),
        NativeMcpMessageKind::Notification => Ok(Message::Notification(Notification {
            jsonrpc,
            method: jsonrpc_method(projection)?,
            params: optional_payload_member(projection, "params")?,
        })),
        NativeMcpMessageKind::ResultResponse => Ok(Message::Response(Response {
            jsonrpc,
            id: Some(id_member(projection, "id")?),
            result: Some(payload_member(projection, "result")?),
            error: None,
        })),
        NativeMcpMessageKind::ErrorResponse => {
            let id = if projection.get_property("id").is_some() {
                Some(id_member(projection, "id")?)
            } else {
                None
            };
            let error = member_projection(projection, "error")?;
            let code = error
                .get_property("code")
                .and_then(JsonValueView::as_number)
                .ok_or_else(|| NativeMcpMessageDecodeError::invalid("error.code is missing"))?
                as i64;
            let message = member_projection(&error, "message")?
                .string_units("")
                .ok_or_else(|| NativeMcpMessageDecodeError::invalid("error.message is missing"))?;
            let data = if error.get_property("data").is_some() {
                Some(payload_member(&error, "data")?)
            } else {
                None
            };
            Ok(Message::Response(Response {
                jsonrpc,
                id,
                result: None,
                error: Some(ResponseError {
                    code,
                    message: JsonRpcString::from_code_units(message),
                    data,
                }),
            }))
        }
    }
}

fn id_member(
    projection: &Utf16JsonProjection,
    key: &str,
) -> Result<Id, NativeMcpMessageDecodeError> {
    let value = member_projection(projection, key)?;
    if let Some(units) = value.string_units("") {
        return Ok(match String::from_utf16(&units) {
            Ok(value) => Id::String(value),
            Err(_) => Id::StringUnits(units),
        });
    }
    let number = value
        .number_at("")
        .or_else(|| value.at_pointer("").and_then(JsonValueView::as_number))
        .ok_or_else(|| {
            NativeMcpMessageDecodeError::invalid("id must be a string or safe integer")
        })?;
    // Native validation already checked Number.isSafeInteger, whose range is
    // strictly within i64.
    Ok(Id::Number(number as i64))
}

fn jsonrpc_method(
    projection: &Utf16JsonProjection,
) -> Result<JsonRpcString, NativeMcpMessageDecodeError> {
    let method = member_projection(projection, "method")?;
    let units = method
        .string_units("")
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("method must be a string"))?;
    Ok(JsonRpcString::from_code_units(units))
}

fn optional_payload_member(
    projection: &Utf16JsonProjection,
    key: &str,
) -> Result<Option<JsonPayload>, NativeMcpMessageDecodeError> {
    if projection.get_property(key).is_some() {
        Ok(Some(payload_member(projection, key)?))
    } else {
        Ok(None)
    }
}

fn payload_member(
    projection: &Utf16JsonProjection,
    key: &str,
) -> Result<JsonPayload, NativeMcpMessageDecodeError> {
    let member = member_projection(projection, key)?;
    let json = member
        .to_json_string()
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
    JsonPayload::parse(json).map_err(|error| NativeMcpMessageDecodeError(error.to_string()))
}

fn member_projection(
    projection: &Utf16JsonProjection,
    key: &str,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    projection
        .subprojection_property_units(&key.encode_utf16().collect::<Vec<_>>())
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))
}

fn normalize_native_projection(
    decoded: &DecodedNativeMcpMessage,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    match decoded.kind {
        NativeMcpMessageKind::Request => normalize_object(
            &decoded.projection,
            &["jsonrpc", "id", "method", "params"],
            false,
            |key, value| {
                if key == "params" {
                    normalize_params(&value)
                } else {
                    Ok(value)
                }
            },
        ),
        NativeMcpMessageKind::Notification => normalize_object(
            &decoded.projection,
            &["jsonrpc", "method", "params"],
            false,
            |key, value| {
                if key == "params" {
                    normalize_params(&value)
                } else {
                    Ok(value)
                }
            },
        ),
        NativeMcpMessageKind::ResultResponse => normalize_object(
            &decoded.projection,
            &["jsonrpc", "id", "result"],
            false,
            |key, value| {
                if key == "result" {
                    normalize_result(&value)
                } else {
                    Ok(value)
                }
            },
        ),
        NativeMcpMessageKind::ErrorResponse => normalize_object(
            &decoded.projection,
            &["jsonrpc", "id", "error"],
            false,
            |key, value| {
                if key == "error" {
                    normalize_object(&value, &["code", "message", "data"], false, |_, value| {
                        Ok(value)
                    })
                } else {
                    Ok(value)
                }
            },
        ),
    }
}

/// Recreate Zod's object output: schema keys first in shape order, followed
/// by passthrough keys in their JavaScript property order. JSON projection
/// serialization applies JavaScript's integer-key ordering after insertion.
fn normalize_object(
    source: &Utf16JsonProjection,
    shape: &[&str],
    passthrough: bool,
    mut normalize_known: impl FnMut(
        &str,
        Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError>,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    let mut fields = Vec::new();
    let mut shape_units = Vec::with_capacity(shape.len());
    for key in shape {
        let units = key.encode_utf16().collect::<Vec<_>>();
        shape_units.push(units.clone());
        if source.get_property(key).is_some() {
            let value = source
                .subprojection_property_units(&units)
                .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
            fields.push((units, normalize_known(key, value)?));
        }
    }
    if passthrough {
        let entries = source
            .at_pointer("")
            .and_then(JsonValueView::object_entries)
            .ok_or_else(|| NativeMcpMessageDecodeError::invalid("expected an object"))?;
        for entry in entries {
            if shape_units.iter().any(|key| key == entry.code_units) {
                continue;
            }
            let value = source
                .subprojection_property_units(entry.code_units)
                .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
            fields.push((entry.code_units.to_vec(), value));
        }
    }
    Utf16JsonProjection::object_units(fields)
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))
}

fn normalize_params(
    source: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    normalize_object(source, &["_meta"], true, |key, value| {
        if key == "_meta" {
            normalize_request_meta(&value)
        } else {
            Ok(value)
        }
    })
}

fn normalize_request_meta(
    source: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    normalize_object(
        source,
        &["progressToken", RELATED_TASK_META_KEY],
        true,
        |key, value| {
            if key == RELATED_TASK_META_KEY {
                normalize_object(&value, &["taskId"], false, |_, value| Ok(value))
            } else {
                Ok(value)
            }
        },
    )
}

fn normalize_result(
    source: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    normalize_object(source, &["_meta"], true, |key, value| {
        if key == "_meta" {
            normalize_result_meta(&value)
        } else {
            Ok(value)
        }
    })
}

fn normalize_result_meta(
    source: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    let server_info_key = "io.modelcontextprotocol/serverInfo";
    let server_info_units = server_info_key.encode_utf16().collect::<Vec<_>>();
    let mut fields = Vec::new();
    if source.get_property(server_info_key).is_some() {
        let value = source
            .subprojection_property_units(&server_info_units)
            .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
        // Native wraps this optional field in `.catch(undefined)`: an invalid
        // serverInfo is stripped while the containing result remains valid.
        if let Some(value) = normalize_server_info(&value)? {
            fields.push((server_info_units.clone(), value));
        }
    }
    let entries = source
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("expected result _meta object"))?;
    for entry in entries {
        if entry.code_units == server_info_units {
            continue;
        }
        let value = source
            .subprojection_property_units(entry.code_units)
            .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
        fields.push((entry.code_units.to_vec(), value));
    }
    Utf16JsonProjection::object_units(fields)
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))
}

fn normalize_server_info(
    source: &Utf16JsonProjection,
) -> Result<Option<Utf16JsonProjection>, NativeMcpMessageDecodeError> {
    if !valid_server_info(source) {
        return Ok(None);
    }
    normalize_object(
        source,
        &[
            "name",
            "title",
            "icons",
            "version",
            "websiteUrl",
            "description",
        ],
        false,
        |key, value| {
            if key == "icons" {
                normalize_server_icons(&value)
            } else {
                Ok(value)
            }
        },
    )
    .map(Some)
}

fn valid_server_info(source: &Utf16JsonProjection) -> bool {
    if !is_string_projection_member(source, "name")
        || !is_string_projection_member(source, "version")
    {
        return false;
    }
    for key in ["title", "websiteUrl", "description"] {
        if source.get_property(key).is_some() && !is_string_projection_member(source, key) {
            return false;
        }
    }
    let Some(icons) = source.get_property("icons") else {
        return true;
    };
    let Some((_, entries)) = icons.array_entries() else {
        return false;
    };
    entries.into_iter().all(|(_, icon)| {
        let Some(icon) = icon.object_entries().map(|_| icon) else {
            return false;
        };
        if !is_string_member_view(icon, "src") {
            return false;
        }
        if has(icon, "mimeType") && !is_string_member_view(icon, "mimeType") {
            return false;
        }
        if has(icon, "sizes") {
            let Some((_, sizes)) = member(icon, "sizes").and_then(JsonValueView::array_entries)
            else {
                return false;
            };
            if sizes
                .into_iter()
                .any(|(_, size)| size.code_units().is_none())
            {
                return false;
            }
        }
        if has(icon, "theme")
            && !member(icon, "theme").is_some_and(|theme| {
                is_string_literal(theme, "light") || is_string_literal(theme, "dark")
            })
        {
            return false;
        }
        true
    })
}

fn normalize_server_icons(
    source: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    let Some((length, _)) = source.array_entries() else {
        return Err(NativeMcpMessageDecodeError::invalid(
            "serverInfo.icons must be an array",
        ));
    };
    let mut icons = Vec::with_capacity(length as usize);
    for index in 0..length {
        let pointer = format!("/{index}");
        let icon = source
            .subprojection(&pointer)
            .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?;
        let normalized = normalize_object(
            &icon,
            &["src", "mimeType", "sizes", "theme"],
            false,
            |key, value| {
                if key == "sizes" {
                    normalize_server_icon_sizes(&value)
                } else {
                    Ok(value)
                }
            },
        )?;
        icons.push(normalized);
    }
    Utf16JsonProjection::array(icons)
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))
}

fn normalize_server_icon_sizes(
    source: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, NativeMcpMessageDecodeError> {
    let Some((length, _)) = source.array_entries() else {
        return Err(NativeMcpMessageDecodeError::invalid(
            "icon sizes must be an array",
        ));
    };
    let mut sizes = Vec::with_capacity(length as usize);
    for index in 0..length {
        sizes.push(
            source
                .subprojection(&format!("/{index}"))
                .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))?,
        );
    }
    Utf16JsonProjection::array(sizes)
        .map_err(|error| NativeMcpMessageDecodeError(error.to_string()))
}

fn is_string_member_view(value: JsonValueView<'_>, key: &str) -> bool {
    member(value, key).is_some_and(is_string)
}

fn is_string_projection_member(value: &Utf16JsonProjection, key: &str) -> bool {
    value.get_property(key).is_some_and(is_string)
}

fn validate_request(root: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    validate_strict_object(root, &["jsonrpc", "id", "method", "params"])?;
    validate_version(root)?;
    require_id(root, "id")?;
    require_string(root, "method")?;
    validate_optional_params(root)?;
    Ok(())
}

fn validate_notification(root: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    validate_strict_object(root, &["jsonrpc", "method", "params"])?;
    validate_version(root)?;
    require_string(root, "method")?;
    validate_optional_params(root)?;
    Ok(())
}

fn validate_result_response(root: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    validate_strict_object(root, &["jsonrpc", "id", "result"])?;
    validate_version(root)?;
    require_id(root, "id")?;
    let result = member(root, "result")
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("result response is missing result"))?;
    validate_mcp_result_object(result)
}

fn validate_error_response(root: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    validate_strict_object(root, &["jsonrpc", "id", "error"])?;
    validate_version(root)?;
    if has(root, "id") {
        require_id(root, "id")?;
    }

    let error = member(root, "error")
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("error response is missing error"))?;
    // The outer error response is strict, but the nested `error` uses a
    // default Zod object (unknown nested keys are accepted and stripped).
    if error.object_entries().is_none() {
        return Err(NativeMcpMessageDecodeError::invalid(
            "error must be an object",
        ));
    }
    let code = member(error, "code")
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("error.code is required"))?;
    if !is_native_integer(code) {
        return Err(NativeMcpMessageDecodeError::invalid(
            "error.code must be a safe integer",
        ));
    }
    require_string(error, "message")?;
    // `data` uses Zod unknown().optional(): every JSON value, including null,
    // is accepted when the property is present.
    Ok(())
}

fn validate_optional_params(root: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    let Some(params) = member(root, "params") else {
        return Ok(());
    };
    if params.object_entries().is_none() {
        return Err(NativeMcpMessageDecodeError::invalid(
            "params must be an object when present",
        ));
    }
    if let Some(meta) = member(params, "_meta") {
        validate_request_meta(meta)?;
    }
    Ok(())
}

fn validate_request_meta(meta: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    if meta.object_entries().is_none() {
        return Err(NativeMcpMessageDecodeError::invalid(
            "params._meta must be an object when present",
        ));
    }
    if let Some(token) = member(meta, "progressToken") {
        if !is_string(token) && !is_native_integer(token) {
            return Err(NativeMcpMessageDecodeError::invalid(
                "params._meta.progressToken must be a string or safe integer",
            ));
        }
    }
    if let Some(related_task) = member(meta, RELATED_TASK_META_KEY) {
        if related_task.object_entries().is_none() || !is_string_member(related_task, "taskId") {
            return Err(NativeMcpMessageDecodeError::invalid(
                "params._meta related-task must be an object with a string taskId",
            ));
        }
    }
    Ok(())
}

fn validate_mcp_result_object(
    result: JsonValueView<'_>,
) -> Result<(), NativeMcpMessageDecodeError> {
    if result.object_entries().is_none() {
        return Err(NativeMcpMessageDecodeError::invalid(
            "result must be an object",
        ));
    }
    // The MCP SDK's result object is loose, but its optional `_meta` member is
    // a metadata object. `serverInfo` within it is caught to undefined when
    // invalid, so no stricter validation is applied to that value here.
    if let Some(meta) = member(result, "_meta") {
        if meta.object_entries().is_none() {
            return Err(NativeMcpMessageDecodeError::invalid(
                "result._meta must be an object when present",
            ));
        }
    }
    Ok(())
}

fn validate_version(root: JsonValueView<'_>) -> Result<(), NativeMcpMessageDecodeError> {
    let version = member(root, "jsonrpc")
        .ok_or_else(|| NativeMcpMessageDecodeError::invalid("jsonrpc is required"))?;
    if !is_string_literal(version, "2.0") {
        return Err(NativeMcpMessageDecodeError::invalid(
            "jsonrpc must equal 2.0",
        ));
    }
    Ok(())
}

fn require_id(
    root: JsonValueView<'_>,
    key: &'static str,
) -> Result<(), NativeMcpMessageDecodeError> {
    let value =
        member(root, key).ok_or_else(|| NativeMcpMessageDecodeError::invalid("id is required"))?;
    if !is_string(value) && !is_native_integer(value) {
        return Err(NativeMcpMessageDecodeError::invalid(
            "id must be a string or safe integer",
        ));
    }
    Ok(())
}

fn require_string(
    root: JsonValueView<'_>,
    key: &'static str,
) -> Result<(), NativeMcpMessageDecodeError> {
    if !is_string_member(root, key) {
        return Err(NativeMcpMessageDecodeError::invalid(
            "required string field is missing or invalid",
        ));
    }
    Ok(())
}

fn validate_strict_object(
    value: JsonValueView<'_>,
    allowed: &[&str],
) -> Result<(), NativeMcpMessageDecodeError> {
    let Some(entries) = value.object_entries() else {
        return Err(NativeMcpMessageDecodeError::invalid(
            "message variant must be an object",
        ));
    };
    if entries.iter().any(|entry| {
        !allowed
            .iter()
            .any(|key| entry.code_units.iter().copied().eq(key.encode_utf16()))
    }) {
        return Err(NativeMcpMessageDecodeError::invalid(
            "message variant contains an unknown field",
        ));
    }
    Ok(())
}

fn member<'a>(root: JsonValueView<'a>, key: &str) -> Option<JsonValueView<'a>> {
    root.get_property(key)
}

fn has(root: JsonValueView<'_>, key: &str) -> bool {
    member(root, key).is_some()
}

fn is_string_member(root: JsonValueView<'_>, key: &str) -> bool {
    member(root, key).is_some_and(is_string)
}

fn is_string(value: JsonValueView<'_>) -> bool {
    value.code_units().is_some()
}

fn is_string_literal(value: JsonValueView<'_>, expected: &str) -> bool {
    value
        .code_units()
        .is_some_and(|actual| actual.iter().copied().eq(expected.encode_utf16()))
}

fn is_native_integer(value: JsonValueView<'_>) -> bool {
    value.as_number().is_some_and(|number| {
        number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER
    })
}

#[cfg(test)]
mod tests {
    use super::{decode_native_mcp_frame, decode_native_mcp_message, NativeMcpMessageKind};
    use jsonrpc::{Id, Message};

    #[test]
    fn decodes_all_four_native_message_variants() {
        let cases = [
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"roots/list","params":{"extra":null,"_meta":{"progressToken":1,"io.modelcontextprotocol/related-task":{"taskId":"t","extra":true}}}}"#,
                NativeMcpMessageKind::Request,
            ),
            (
                r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
                NativeMcpMessageKind::Notification,
            ),
            (
                r#"{"jsonrpc":"2.0","id":"call-1","result":{"value":null,"_meta":{"future":"ok"}}}"#,
                NativeMcpMessageKind::ResultResponse,
            ),
            (
                r#"{"jsonrpc":"2.0","error":{"code":-2147483649,"message":"failed","data":null,"unknownNested":true}}"#,
                NativeMcpMessageKind::ErrorResponse,
            ),
        ];
        for (source, expected) in cases {
            let decoded = decode_native_mcp_message(source).expect("schema-valid source");
            assert_eq!(decoded.kind, expected);
        }
    }

    #[test]
    fn rejects_invalid_result_error_and_missing_response_members() {
        for source in [
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":null}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"x"}}"#,
        ] {
            assert!(decode_native_mcp_message(source).is_err(), "{source}");
        }
    }

    #[test]
    fn matches_safe_integer_and_json_number_semantics() {
        for source in [
            r#"{"jsonrpc":"2.0","id":9007199254740991,"method":"m"}"#,
            r#"{"jsonrpc":"2.0","id":12.0,"method":"m"}"#,
            r#"{"jsonrpc":"2.0","error":{"code":12.0,"message":"x","data":null}}"#,
            r#"{"jsonrpc":"2.0","error":{"code":2147483648,"message":"x"}}"#,
        ] {
            assert!(decode_native_mcp_message(source).is_ok(), "{source}");
        }
        for source in [
            r#"{"jsonrpc":"2.0","id":9007199254740992,"method":"m"}"#,
            r#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#,
            r#"{"jsonrpc":"2.0","error":{"code":9007199254740992,"message":"x"}}"#,
        ] {
            assert!(decode_native_mcp_message(source).is_err(), "{source}");
        }
    }

    #[test]
    fn duplicate_keys_use_last_json_parse_value_and_strings_keep_utf16() {
        let decoded = decode_native_mcp_message(
            r#"{"jsonrpc":"1.0","jsonrpc":"2.0","id":1,"method":"m\ud800"}"#,
        )
        .expect("last duplicate value wins and lone surrogate is a JS string");
        assert_eq!(decoded.kind, NativeMcpMessageKind::Request);
        let method = decoded
            .projection
            .at_pointer("")
            .and_then(|root| root.get_property("method"))
            .expect("method");
        assert_eq!(method.code_units().unwrap().last(), Some(&0xd800));
    }

    #[test]
    fn strict_envelopes_and_non_null_meta_requirements_are_enforced() {
        for source in [
            r#"{"jsonrpc":"2.0","id":1,"method":"m","extra":true}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"m","params":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"m","params":{"_meta":null}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"_meta":null}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":1,"message":"x"}}"#,
        ] {
            assert!(decode_native_mcp_message(source).is_err(), "{source}");
        }
    }

    #[test]
    fn frame_decoder_normalizes_loose_params_in_native_schema_order() {
        let source = r#"{"jsonrpc":"2.0","params":{"tail":1,"_meta":{"tailMeta":2,"io.modelcontextprotocol/related-task":{"extra":true,"taskId":"t"},"progressToken":3},"first":0},"method":"roots/list","id":9007199254740991}"#;
        let Message::Request(request) = decode_native_mcp_frame(source.as_bytes()).unwrap() else {
            panic!("expected request")
        };
        assert_eq!(request.id, Id::Number(9_007_199_254_740_991));
        let params = request.params.unwrap();
        assert_eq!(
            params.as_json(),
            r#"{"_meta":{"progressToken":3,"io.modelcontextprotocol/related-task":{"taskId":"t"},"tailMeta":2},"tail":1,"first":0}"#
        );
    }

    #[test]
    fn frame_decoder_preserves_utf16_payloads_and_wide_safe_integer_error_codes() {
        let source = r#"{"jsonrpc":"2.0","error":{"unknownNested":true,"message":"failed\ud800","code":9007199254740991,"data":null}}"#;
        let Message::Response(response) = decode_native_mcp_frame(source.as_bytes()).unwrap()
        else {
            panic!("expected error response")
        };
        let error = response.error.unwrap();
        assert_eq!(error.code, 9_007_199_254_740_991);
        assert_eq!(error.message.code_units().last(), Some(&0xd800));
        assert_eq!(error.data.unwrap().as_json(), "null");
        assert!(response.id.is_none());
    }

    #[test]
    fn frame_decoder_normalizes_result_server_info_and_catches_invalid_value() {
        let source = r#"{"jsonrpc":"2.0","id":1,"result":{"z":1,"_meta":{"zmeta":2,"io.modelcontextprotocol/serverInfo":{"version":"v","extra":true,"name":"n","icons":[{"src":"x","extra":"strip","theme":"light"}]}},"a":2}}"#;
        let Message::Response(response) = decode_native_mcp_frame(source.as_bytes()).unwrap()
        else {
            panic!("expected result response")
        };
        assert_eq!(
            response.result.unwrap().as_json(),
            r#"{"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"n","icons":[{"src":"x","theme":"light"}],"version":"v"},"zmeta":2},"z":1,"a":2}"#
        );

        let invalid = r#"{"jsonrpc":"2.0","id":1,"result":{"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"n","version":null},"keep":true}}}"#;
        let Message::Response(response) = decode_native_mcp_frame(invalid.as_bytes()).unwrap()
        else {
            panic!("expected result response")
        };
        assert_eq!(
            response.result.unwrap().as_json(),
            r#"{"_meta":{"keep":true}}"#
        );
    }

    #[test]
    fn frame_decoder_preserves_unpaired_method_code_units() {
        let source = r#"{"jsonrpc":"2.0","id":1,"method":"m\ud800"}"#;
        let Message::Request(request) = decode_native_mcp_frame(source.as_bytes()).unwrap() else {
            panic!("expected request")
        };
        assert_eq!(request.method.code_units(), &[u16::from(b'm'), 0xd800]);
    }
}
