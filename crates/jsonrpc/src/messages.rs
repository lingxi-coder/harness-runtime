//! JSON-RPC 2.0 wire types — `Id`, `Request`, `Response`, `Notification`,
//! and the `Message` envelope used by codecs.
//!
//! The shapes mirror <https://www.jsonrpc.org/specification> exactly. The
//! `#[serde(untagged)]` `Message` enum disambiguates inbound traffic by
//! field presence (`id`+`method` → Request, `id`+`result`/`error` →
//! Response, `method` only → Notification).

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

fn deserialize_present_json_payload<'de, D>(
    deserializer: D,
) -> Result<Option<JsonPayload>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    JsonPayload::parse(raw.get())
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// A validated JSON source value. The raw JSON text is retained as the
/// transport carrier so arbitrary string code units never pass through a
/// recursive `serde_json::Value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPayload(String);

impl JsonPayload {
    /// Validate and retain one raw JSON value.
    pub fn parse(source: impl Into<String>) -> Result<Self, serde_json::Error> {
        let source = source.into();
        RawValue::from_string(source.clone())?;
        Ok(Self(source))
    }

    /// Serialize a typed, host-generated value into the raw JSON carrier.
    pub fn from_serializable(value: &impl Serialize) -> Result<Self, serde_json::Error> {
        Self::parse(serde_json::to_string(value)?)
    }

    /// Retained JSON text, including exact escape spelling from decoded wire
    /// frames.
    #[must_use]
    pub fn as_json(&self) -> &str {
        &self.0
    }

    /// Consume the carrier and return its validated JSON text.
    #[must_use]
    pub fn into_json(self) -> String {
        self.0
    }

    /// Explicitly materialize a recursive Value for bounded control consumers.
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.0)
    }
}

impl Serialize for JsonPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RawValue::from_string(self.0.clone())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        Ok(Self(raw.get().to_owned()))
    }
}

/// Literal `"2.0"` — the only valid value for the `jsonrpc` field per spec.
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC 2.0 reserved error code: invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC 2.0 reserved error code: the JSON sent is not a valid Request object.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC 2.0 reserved error code: method does not exist or is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 2.0 reserved error code: invalid method parameter(s).
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 2.0 reserved error code: internal JSON-RPC error.
pub const INTERNAL_ERROR: i64 = -32603;

/// JSON-RPC 2.0 request/response identifier. Permitted shapes per spec are
/// number, string, or null; this enum models the two non-null cases. A null
/// id on a *response* is represented by `Response.id == None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    /// Numeric id (we use this for outbound calls — monotonic i64 from the router).
    Number(i64),
    /// String id (used by some peers; mirrored verbatim when we echo responses).
    String(String),
    /// Exact UTF-16 string id when the peer supplied an unpaired surrogate.
    /// JSON-RPC identifiers are opaque; retaining their code units prevents a
    /// response from changing the identifier while passing through a local
    /// request handler.
    StringUnits(Vec<u16>),
}

impl Serialize for Id {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Number(value) => serializer.serialize_i64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::StringUnits(units) => {
                let raw = RawValue::from_string(quote_utf16_json_string(units))
                    .map_err(serde::ser::Error::custom)?;
                raw.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let source = raw.get();
        if source.starts_with('"') {
            let units = parse_json_string_units(source).map_err(serde::de::Error::custom)?;
            return Ok(match String::from_utf16(&units) {
                Ok(value) => Self::String(value),
                Err(_) => Self::StringUnits(units),
            });
        }
        serde_json::from_str::<i64>(source)
            .map(Self::Number)
            .map_err(serde::de::Error::custom)
    }
}

fn quote_utf16_json_string(units: &[u16]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(units.len() + 2);
    output.push('"');
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        match unit {
            0x22 => output.push_str("\\\""),
            0x5c => output.push_str("\\\\"),
            0x08 => output.push_str("\\b"),
            0x0c => output.push_str("\\f"),
            0x0a => output.push_str("\\n"),
            0x0d => output.push_str("\\r"),
            0x09 => output.push_str("\\t"),
            0x00..=0x1f => write!(output, "\\u{unit:04x}").expect("String writes do not fail"),
            0xd800..=0xdbff
                if units
                    .get(index + 1)
                    .is_some_and(|next| (0xdc00..=0xdfff).contains(next)) =>
            {
                let low = units[index + 1];
                let scalar =
                    0x10000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
                output.push(char::from_u32(scalar).expect("valid surrogate pair"));
                index += 1;
            }
            0xd800..=0xdfff => write!(output, "\\u{unit:04x}").expect("String writes do not fail"),
            _ => output.push(char::from_u32(u32::from(unit)).expect("non-surrogate UTF-16 unit")),
        }
        index += 1;
    }
    output.push('"');
    output
}

fn parse_json_string_units(source: &str) -> Result<Vec<u16>, &'static str> {
    let bytes = source.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || *bytes.last().expect("length checked") != b'"' {
        return Err("expected a quoted JSON string");
    }
    let mut units = Vec::new();
    let mut index = 1;
    while index < bytes.len() - 1 {
        match bytes[index] {
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index).ok_or("incomplete JSON string escape")?;
                match escaped {
                    b'"' => units.push(0x22),
                    b'\\' => units.push(0x5c),
                    b'/' => units.push(0x2f),
                    b'b' => units.push(0x08),
                    b'f' => units.push(0x0c),
                    b'n' => units.push(0x0a),
                    b'r' => units.push(0x0d),
                    b't' => units.push(0x09),
                    b'u' => {
                        let digits = bytes
                            .get(index + 1..index + 5)
                            .ok_or("incomplete unicode escape")?;
                        let digits =
                            std::str::from_utf8(digits).map_err(|_| "invalid unicode escape")?;
                        let unit = u16::from_str_radix(digits, 16)
                            .map_err(|_| "invalid unicode escape")?;
                        units.push(unit);
                        index += 4;
                    }
                    _ => return Err("invalid JSON string escape"),
                }
                index += 1;
            }
            _ => {
                let source_tail = std::str::from_utf8(&bytes[index..bytes.len() - 1])
                    .map_err(|_| "invalid UTF-8 in JSON string id")?;
                let character = source_tail
                    .chars()
                    .next()
                    .ok_or("unterminated JSON string")?;
                units.extend(character.encode_utf16(&mut [0; 2]).iter().copied());
                index += character.len_utf8();
            }
        }
    }
    Ok(units)
}

/// One JSON string with exact JavaScript UTF-16 code units.
///
/// This is the typed string carrier for wire fields whose contents may come
/// from an arbitrary JSON-RPC peer. `display()` is a lossy convenience for
/// logs and UI boundaries; serialization preserves the original code units.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonRpcString(Vec<u16>);

impl JsonRpcString {
    /// Build a source string from exact UTF-16 code units.
    #[must_use]
    pub fn from_code_units(code_units: Vec<u16>) -> Self {
        Self(code_units)
    }

    /// Borrow the exact UTF-16 code units.
    #[must_use]
    pub fn code_units(&self) -> &[u16] {
        &self.0
    }

    /// Convert to a display-safe Rust string. This may replace unpaired
    /// surrogates and is intended only for display or logging.
    #[must_use]
    pub fn display(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
}

impl From<String> for JsonRpcString {
    fn from(value: String) -> Self {
        Self(value.encode_utf16().collect())
    }
}

impl From<&str> for JsonRpcString {
    fn from(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }
}

impl fmt::Display for JsonRpcString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display())
    }
}

/// Compare a Rust string to exact UTF-16 units, without using lossy display.
impl PartialEq<&str> for JsonRpcString {
    fn eq(&self, other: &&str) -> bool {
        self.0.iter().copied().eq(other.encode_utf16())
    }
}

impl Serialize for JsonRpcString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RawValue::from_string(quote_utf16_json_string(&self.0))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonRpcString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        parse_json_string_units(raw.get())
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// JSON-RPC 2.0 Request — has both `method` and `id`.
///
/// `deny_unknown_fields` is required so the `#[serde(untagged)] Message` enum
/// can disambiguate by field-presence in the documented order (Request →
/// Response → Notification). Without it, a Notification payload would parse
/// as a Request with `method` rejected and then as a Response (with `method`
/// silently dropped), defeating the precedence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// MUST equal `"2.0"`.
    pub jsonrpc: String,
    /// Request id.
    pub id: Id,
    /// Method name (e.g. `"initialize"`, `"tools/list"`) with exact UTF-16
    /// code units retained for JavaScript-compatible peer strings.
    pub method: JsonRpcString,
    /// Optional parameters — serialized as either an object or an array.
    #[serde(
        default,
        deserialize_with = "deserialize_present_json_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub params: Option<JsonPayload>,
}

/// JSON-RPC 2.0 Notification — has `method` but NO `id`.
///
/// See `Request` for the `deny_unknown_fields` rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// MUST equal `"2.0"`.
    pub jsonrpc: String,
    /// Method name with exact UTF-16 code units retained for
    /// JavaScript-compatible peer strings.
    pub method: JsonRpcString,
    /// Optional parameters.
    #[serde(
        default,
        deserialize_with = "deserialize_present_json_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub params: Option<JsonPayload>,
}

/// JSON-RPC 2.0 Response — has `id`, plus exactly one of `result` or `error`.
/// `id` may be `null` if the peer's request was unparseable.
///
/// See `Request` for the `deny_unknown_fields` rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// MUST equal `"2.0"`.
    pub jsonrpc: String,
    /// Echoed request id, or `null` if id was unrecoverable.
    pub id: Option<Id>,
    /// Result on success.
    #[serde(
        default,
        deserialize_with = "deserialize_present_json_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<JsonPayload>,
    /// Error on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// Error payload inside a `Response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    /// One of the JSON-RPC 2.0 codes or an application-defined code.
    pub code: i64,
    /// Human-readable error.
    pub message: JsonRpcString,
    /// Optional remote source data. This is retained as raw JSON so generic
    /// error payloads do not pass through a recursive `Value` tree.
    #[serde(
        default,
        deserialize_with = "deserialize_present_json_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<JsonPayload>,
}

/// Inbound message envelope — used by codecs to deliver one of three shapes.
///
/// Variant order matters for `#[serde(untagged)]` matching: `Request` (has
/// `id` AND `method`) is tried first, then `Response` (has `id` and either
/// `result` or `error`), then `Notification` (has `method` only). Do not
/// reorder without updating the parser tests.
#[derive(Debug, Clone)]
pub enum Message {
    /// Peer request — dispatch to an `InboundHandler`.
    Request(Request),
    /// Response to an outbound request — route to the pending oneshot.
    Response(Response),
    /// Peer notification — fan out via the notification broker.
    Notification(Notification),
}

impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Dispatch directly to the concrete envelope serializer. An untagged
        // derive buffers the object in `Content`, which cannot retain RawValue
        // source members on all serializer paths.
        match self {
            Self::Request(request) => request.serialize(serializer),
            Self::Response(response) => response.serialize(serializer),
            Self::Notification(notification) => notification.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MessageVisitor;

        impl<'de> Visitor<'de> for MessageVisitor {
            type Value = Message;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON-RPC message object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut seen = HashSet::<String>::new();
                let mut jsonrpc = None;
                let mut id_seen = false;
                let mut id = None;
                let mut method = None;
                let mut params = None;
                let mut result = None;
                let mut error = None;
                let mut unknown = Vec::new();

                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON-RPC field {key:?}"
                        )));
                    }
                    match key.as_str() {
                        "jsonrpc" => jsonrpc = Some(map.next_value::<String>()?),
                        "id" => {
                            id_seen = true;
                            id = map.next_value::<Option<Id>>()?;
                        }
                        "method" => method = Some(map.next_value::<JsonRpcString>()?),
                        "params" => params = Some(map.next_value::<JsonPayload>()?),
                        "result" => result = Some(map.next_value::<JsonPayload>()?),
                        "error" => error = Some(map.next_value::<ResponseError>()?),
                        _ => {
                            unknown.push(key);
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }

                let jsonrpc = jsonrpc.ok_or_else(|| serde::de::Error::missing_field("jsonrpc"))?;
                let reject_unknown = |allowed: &'static [&'static str]| -> Result<(), M::Error> {
                    if let Some(field) = unknown.first() {
                        return Err(serde::de::Error::unknown_field(field, allowed));
                    }
                    for field in &seen {
                        if !allowed.contains(&field.as_str()) {
                            return Err(serde::de::Error::unknown_field(field, allowed));
                        }
                    }
                    Ok(())
                };

                if method.is_some() && id_seen && id.is_some() {
                    reject_unknown(&["jsonrpc", "id", "method", "params"])?;
                    return Ok(Message::Request(Request {
                        jsonrpc,
                        id: id.expect("checked request id"),
                        method: method.expect("checked method"),
                        params,
                    }));
                }
                if method.is_some() && !id_seen {
                    reject_unknown(&["jsonrpc", "method", "params"])?;
                    return Ok(Message::Notification(Notification {
                        jsonrpc,
                        method: method.expect("checked method"),
                        params,
                    }));
                }
                if method.is_none() && id_seen && (result.is_some() || error.is_some()) {
                    reject_unknown(&["jsonrpc", "id", "result", "error"])?;
                    return Ok(Message::Response(Response {
                        jsonrpc,
                        id,
                        result,
                        error,
                    }));
                }
                Err(serde::de::Error::custom(
                    "JSON-RPC message has no recognized request, response, or notification shape",
                ))
            }
        }

        deserializer.deserialize_map(MessageVisitor)
    }
}

impl Request {
    /// Build a fresh request with `jsonrpc = "2.0"`.
    pub fn new(method: impl Into<JsonRpcString>, params: Option<JsonPayload>, id: Id) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            method: method.into(),
            params,
        }
    }
}

impl Notification {
    /// Build a fresh notification with `jsonrpc = "2.0"`.
    pub fn new(method: impl Into<JsonRpcString>, params: Option<JsonPayload>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params,
        }
    }
}

impl Response {
    /// Build a success response.
    #[must_use]
    pub fn success(id: Id, result: impl Serialize) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id),
            result: Some(
                JsonPayload::from_serializable(&result)
                    .expect("a host-generated JSON-RPC result serializes as JSON"),
            ),
            error: None,
        }
    }

    /// Build a success response from validated JSON text without materializing
    /// a recursive `serde_json::Value`. The supplied text is checked now and
    /// serialized later as a raw JSON member.
    #[must_use]
    pub fn success_raw_json(id: Id, result_json: String) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id),
            result: Some(JsonPayload::parse(result_json)?),
            error: None,
        })
    }

    /// Build an error response carrying the original id (if known).
    #[must_use]
    pub fn error(id: Option<Id>, error: ResponseError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jsonrpc_version_constant_is_literally_2_0() {
        assert_eq!(JSONRPC_VERSION, "2.0");
    }

    #[test]
    fn error_code_constants_match_spec() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
    }

    #[test]
    fn id_number_roundtrip() {
        let id = Id::Number(42);
        let v = serde_json::to_value(&id).unwrap();
        assert_eq!(v, json!(42));
        let back: Id = serde_json::from_str(&v.to_string()).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn id_string_roundtrip() {
        let id = Id::String("abc".into());
        let v = serde_json::to_value(&id).unwrap();
        assert_eq!(v, json!("abc"));
        let back: Id = serde_json::from_str(&v.to_string()).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn exact_string_id_keeps_unpaired_utf16_units_through_message_roundtrip() {
        let message: Message = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"\ud800","method":"local/callback","params":{"value":"\udfff"}}"#,
        )
        .expect("parse exact request id and params");
        let Message::Request(request) = message else {
            panic!("expected request")
        };
        assert_eq!(request.id, Id::StringUnits(vec![0xd800]));
        assert_eq!(
            request.params.as_ref().unwrap().as_json(),
            r#"{"value":"\udfff"}"#
        );

        let wire = serde_json::to_string(&Message::Request(request)).unwrap();
        assert!(wire.contains(r#""id":"\ud800""#), "wire: {wire}");
        assert!(wire.contains(r#""value":"\udfff""#), "wire: {wire}");
        assert!(!wire.contains('�'), "wire: {wire}");
    }

    #[test]
    fn request_and_notification_methods_keep_unpaired_utf16_units() {
        let request: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"m\ud800","params":null}"#)
                .unwrap();
        let Message::Request(request) = request else {
            panic!("expected request")
        };
        assert_eq!(request.method.code_units(), &[b'm' as u16, 0xd800]);
        assert_ne!(
            request.method,
            JsonRpcString::from_code_units(vec![b'm' as u16, 0xfffd])
        );
        let request_wire = serde_json::to_string(&Message::Request(request)).unwrap();
        assert!(request_wire.contains(r#""method":"m\ud800""#));
        assert!(!request_wire.contains('�'));

        let notification: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"n\udfff"}"#).unwrap();
        let Message::Notification(notification) = notification else {
            panic!("expected notification")
        };
        assert_eq!(notification.method.code_units(), &[b'n' as u16, 0xdfff]);
        let notification_wire =
            serde_json::to_string(&Message::Notification(notification)).unwrap();
        assert!(notification_wire.contains(r#""method":"n\udfff""#));
        assert!(!notification_wire.contains('�'));
    }

    #[test]
    fn request_wire_shape_has_jsonrpc_id_method_params() {
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Id::Number(1),
            method: "initialize".into(),
            params: Some(
                JsonPayload::from_serializable(&json!({"clientInfo": {"name": "lingxi"}})).unwrap(),
            ),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":1"));
        assert!(s.contains("\"method\":\"initialize\""));
        assert!(s.contains("\"params\":{\"clientInfo\":{\"name\":\"lingxi\"}}"));
        assert!(s.find("\"jsonrpc\"").unwrap() < s.find("\"id\"").unwrap());
        assert!(s.find("\"id\"").unwrap() < s.find("\"method\"").unwrap());
        assert!(s.find("\"method\"").unwrap() < s.find("\"params\"").unwrap());
    }

    #[test]
    fn notification_has_no_id_field_when_serialized() {
        let n = Notification {
            jsonrpc: JSONRPC_VERSION.into(),
            method: "notifications/cancelled".into(),
            params: Some(JsonPayload::from_serializable(&json!({"requestId": 7})).unwrap()),
        };
        let v = serde_json::to_value(&n).unwrap();
        assert!(
            v.get("id").is_none(),
            "notification must not serialize an id field"
        );
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/cancelled");
    }

    #[test]
    fn response_success_serializes_result_not_error() {
        let r = Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(Id::Number(1)),
            result: Some(JsonPayload::from_serializable(&json!({"ok": true})).unwrap()),
            error: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"]["ok"], true);
        assert!(
            v.get("error").is_none(),
            "success response must omit error field"
        );
    }

    #[test]
    fn response_error_serializes_error_not_result() {
        let r = Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(Id::Number(1)),
            result: None,
            error: Some(ResponseError {
                code: METHOD_NOT_FOUND,
                message: "no such method".into(),
                data: None,
            }),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "no such method");
        assert!(
            v.get("result").is_none(),
            "error response must omit result field"
        );
    }

    #[test]
    fn response_error_preserves_native_safe_integer_code_range() {
        for (wire, expected) in [
            (
                r#"{"code":9007199254740991,"message":"high"}"#,
                9_007_199_254_740_991_i64,
            ),
            (
                r#"{"code":-9007199254740991,"message":"low"}"#,
                -9_007_199_254_740_991_i64,
            ),
        ] {
            let error: ResponseError = serde_json::from_str(wire).unwrap();
            assert_eq!(error.code, expected);
            let serialized = serde_json::to_string(&error).unwrap();
            let code_field = format!(r#""code":{expected}"#);
            assert!(serialized.contains(code_field.as_str()));
            let roundtrip: ResponseError = serde_json::from_str(&serialized).unwrap();
            assert_eq!(roundtrip.code, expected);
        }
    }

    #[test]
    fn response_with_null_result_preserves_field_presence_on_decode() {
        let response: Response =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).unwrap();
        assert_eq!(response.result.as_ref().unwrap().as_json(), "null");
        assert!(response.error.is_none());

        let wire = serde_json::to_value(&response).unwrap();
        assert!(wire.get("result").is_some());
        assert!(wire["result"].is_null());
    }

    #[test]
    fn optional_raw_json_members_preserve_explicit_null() {
        let request: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"m","params":null}"#).unwrap();
        assert_eq!(request.params.as_ref().unwrap().as_json(), "null");
        let notification: Notification =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"n","params":null}"#).unwrap();
        assert_eq!(notification.params.as_ref().unwrap().as_json(), "null");
        let error: ResponseError =
            serde_json::from_str(r#"{"code":-1,"message":"error","data":null}"#).unwrap();
        assert_eq!(error.data.as_ref().unwrap().as_json(), "null");
    }

    #[test]
    fn response_error_message_retains_unpaired_utf16_and_serializes_as_json_string() {
        let error: ResponseError =
            serde_json::from_str(r#"{"code":-1,"message":"remote\ud800message"}"#).unwrap();
        let mut expected = "remote".encode_utf16().collect::<Vec<_>>();
        expected.push(0xd800);
        expected.extend("message".encode_utf16());
        assert_eq!(error.message.code_units(), expected.as_slice());
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(serialized.contains(r#""message":"remote\ud800message""#));
        assert!(!serialized.contains('�'));
    }

    #[test]
    fn response_null_id_when_id_unknown() {
        let r = Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: None,
            result: None,
            error: Some(ResponseError {
                code: PARSE_ERROR,
                message: "bad json".into(),
                data: None,
            }),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v["id"].is_null());
    }

    #[test]
    fn message_envelope_disambiguates_request_response_notification() {
        let req: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"m","id":1,"params":{}}"#).unwrap();
        let resp: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        let null_resp: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":2,"result":null}"#).unwrap();
        let noti: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"n","params":{}}"#).unwrap();

        assert!(matches!(req, Message::Request(_)));
        assert!(matches!(resp, Message::Response(_)));
        assert!(matches!(null_resp, Message::Response(_)));
        assert!(matches!(noti, Message::Notification(_)));
    }

    #[test]
    fn id_negative_number_roundtrip() {
        let id = Id::Number(-1);
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "-1");
        let back: Id = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn request_without_params_omits_field() {
        let req = Request::new("ping", None, Id::Number(1));
        let s = serde_json::to_string(&req).unwrap();
        assert!(
            !s.contains("\"params\""),
            "params: None must serialize as no field, got: {s}"
        );
    }

    #[test]
    fn message_parses_request_with_string_id() {
        let m: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"m","id":"abc-123"}"#).unwrap();
        match m {
            Message::Request(r) => assert_eq!(r.id, Id::String("abc-123".into())),
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn response_raw_result_is_validated_and_serialized_as_raw_json() {
        let response =
            Response::success_raw_json(Id::Number(7), r#"{"content":"\ud800"}"#.to_string())
                .expect("valid JSON result");
        let wire = serde_json::to_string(&Message::Response(response)).unwrap();
        assert!(wire.contains(r#""content":"\ud800""#), "wire: {wire}");
        assert!(!wire.contains('�'), "wire: {wire}");
        assert!(Response::success_raw_json(Id::Number(8), "{".to_string()).is_err());
    }

    #[test]
    fn response_constructors_set_jsonrpc_field() {
        let ok = Response::success(Id::Number(1), json!({"x": 1}));
        assert_eq!(ok.jsonrpc, "2.0");
        let err = Response::error(
            Some(Id::Number(2)),
            ResponseError {
                code: INVALID_REQUEST,
                message: "bad".into(),
                data: None,
            },
        );
        assert_eq!(err.jsonrpc, "2.0");
    }
}
