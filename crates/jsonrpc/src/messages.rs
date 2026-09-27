//! JSON-RPC 2.0 wire types — `Id`, `Request`, `Response`, `Notification`,
//! and the `Message` envelope used by codecs.
//!
//! The shapes mirror <https://www.jsonrpc.org/specification> exactly. The
//! `#[serde(untagged)]` `Message` enum disambiguates inbound traffic by
//! field presence (`id`+`method` → Request, `id`+`result`/`error` →
//! Response, `method` only → Notification).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Literal `"2.0"` — the only valid value for the `jsonrpc` field per spec.
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC 2.0 reserved error code: invalid JSON was received.
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC 2.0 reserved error code: the JSON sent is not a valid Request object.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC 2.0 reserved error code: method does not exist or is not available.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC 2.0 reserved error code: invalid method parameter(s).
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC 2.0 reserved error code: internal JSON-RPC error.
pub const INTERNAL_ERROR: i32 = -32603;

/// JSON-RPC 2.0 request/response identifier. Permitted shapes per spec are
/// number, string, or null; this enum models the two non-null cases. A null
/// id on a *response* is represented by `Response.id == None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id (we use this for outbound calls — monotonic i64 from the router).
    Number(i64),
    /// String id (used by some peers; mirrored verbatim when we echo responses).
    String(String),
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
    /// Method name (e.g. `"initialize"`, `"tools/list"`).
    pub method: String,
    /// Optional parameters — serialized as either an object or an array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 Notification — has `method` but NO `id`.
///
/// See `Request` for the `deny_unknown_fields` rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// MUST equal `"2.0"`.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Optional parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
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
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_response_result"
    )]
    pub result: Option<Value>,
    /// Error on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// Error payload inside a `Response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    /// One of the JSON-RPC 2.0 codes or an application-defined code.
    pub code: i32,
    /// Human-readable error.
    pub message: String,
    /// Optional structured data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

fn deserialize_response_result<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// Inbound message envelope — used by codecs to deliver one of three shapes.
///
/// Variant order matters for `#[serde(untagged)]` matching: `Request` (has
/// `id` AND `method`) is tried first, then `Response` (has `id` and either
/// `result` or `error`), then `Notification` (has `method` only). Do not
/// reorder without updating the parser tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// Peer request — dispatch to an `InboundHandler`.
    Request(Request),
    /// Response to an outbound request — route to the pending oneshot.
    Response(Response),
    /// Peer notification — fan out via the notification broker.
    Notification(Notification),
}

impl Request {
    /// Build a fresh request with `jsonrpc = "2.0"`.
    pub fn new(method: impl Into<String>, params: Option<Value>, id: Id) -> Self {
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
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
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
    pub fn success(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
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
        let back: Id = serde_json::from_value(v).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn id_string_roundtrip() {
        let id = Id::String("abc".into());
        let v = serde_json::to_value(&id).unwrap();
        assert_eq!(v, json!("abc"));
        let back: Id = serde_json::from_value(v).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn request_wire_shape_has_jsonrpc_id_method_params() {
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Id::Number(1),
            method: "initialize".into(),
            params: Some(json!({"clientInfo": {"name": "lingxi"}})),
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
            params: Some(json!({"requestId": 7})),
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
            result: Some(json!({"ok": true})),
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
    fn response_with_null_result_preserves_field_presence_on_decode() {
        let response: Response = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        }))
        .unwrap();
        assert_eq!(response.result, Some(Value::Null));
        assert!(response.error.is_none());

        let wire = serde_json::to_value(&response).unwrap();
        assert!(wire.get("result").is_some());
        assert!(wire["result"].is_null());
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
        let req_v = json!({"jsonrpc":"2.0", "method":"m", "id": 1, "params": {}});
        let resp_v = json!({"jsonrpc":"2.0", "id": 1, "result": {"ok": true}});
        let null_resp_v = json!({"jsonrpc":"2.0", "id": 2, "result": null});
        let noti_v = json!({"jsonrpc":"2.0", "method":"n", "params": {}});

        let req: Message = serde_json::from_value(req_v).unwrap();
        let resp: Message = serde_json::from_value(resp_v).unwrap();
        let null_resp: Message = serde_json::from_value(null_resp_v).unwrap();
        let noti: Message = serde_json::from_value(noti_v).unwrap();

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
        let v = json!({"jsonrpc":"2.0", "method":"m", "id":"abc-123"});
        let m: Message = serde_json::from_value(v).unwrap();
        match m {
            Message::Request(r) => assert_eq!(r.id, Id::String("abc-123".into())),
            other => panic!("expected Request, got {other:?}"),
        }
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
