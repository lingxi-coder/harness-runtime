//! HTTP and SSE transport DTOs (no I/O — pure data).

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// HTTP request method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// HTTP `GET`.
    Get,
    /// HTTP `POST`.
    Post,
    /// HTTP `PUT`.
    Put,
    /// HTTP `PATCH`.
    Patch,
    /// HTTP `DELETE`.
    Delete,
    /// HTTP `HEAD`.
    Head,
    /// HTTP `OPTIONS`.
    Options,
}

/// An HTTP request, expressed as pure data (no I/O).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    /// Request method.
    pub method: HttpMethod,
    /// Fully-qualified target URL.
    pub url: String,
    /// Request headers as ordered (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body (typically UTF-8 JSON).
    pub body: Option<String>,
    /// Optional raw request body; takes precedence over `body` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<Vec<u8>>,
    /// Optional overall request timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

/// An HTTP response, expressed as pure data (no I/O).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    /// Numeric HTTP status code.
    pub status: u16,
    /// Response headers as ordered (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// Response body (typically UTF-8 JSON or SSE text).
    ///
    /// For binary responses this is the *lossy* UTF-8 decoding of the wire
    /// bytes (`String::from_utf8_lossy`); consumers that need byte-exact
    /// fidelity (e.g. WebFetch's binary-artifact persist) must read
    /// [`Self::body_bytes`] instead.
    pub body: String,
    /// Raw response-body bytes exactly as received on the wire, *before* any
    /// UTF-8 decoding — mirrors the [`HttpRequest::body_bytes`] precedent.
    ///
    /// Empty when the producer captured only [`Self::body`] (e.g. test mocks
    /// and non-transport producers); production transports populate BOTH
    /// `body` (via `from_utf8_lossy`) and this field, so a genuinely-binary
    /// body (PDF/image/invalid-UTF8) survives byte-identically to consumers
    /// such as WebFetch's raw-artifact save. `skip_serializing_if` keeps the
    /// serialized shape unchanged for producers that leave it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body_bytes: Vec<u8>,
}

/// A single Server-Sent Events frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SseEvent {
    /// Optional event type (the `event:` field).
    pub event_type: Option<String>,
    /// Event payload (the `data:` field).
    pub data: String,
    /// Optional event ID (the `id:` field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_request_roundtrip() {
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "https://api.anthropic.com/v1/messages".into(),
            headers: vec![("authorization".into(), "Bearer xyz".into())],
            body: Some(r#"{"model":"claude-opus-4-6"}"#.into()),
            body_bytes: None,
            timeout: Some(Duration::from_secs(30)),
        };
        let s = serde_json::to_string(&req).unwrap();
        let req2: HttpRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req.method, req2.method);
        assert_eq!(req.url, req2.url);
    }

    #[test]
    fn sse_event_default_type_omitted() {
        let e = SseEvent {
            event_type: None,
            data: "{}".into(),
            id: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("id"));
    }

    #[test]
    fn http_request_body_bytes_none_omitted_from_json() {
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "https://example.com".into(),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(
            !s.contains("body_bytes"),
            "None must be omitted for backward compat; got: {s}"
        );
        // Legacy JSON without the field still deserializes (serde default).
        let legacy = r#"{"method":"GET","url":"https://example.com","headers":[],"body":null}"#;
        let req2: HttpRequest = serde_json::from_str(legacy).unwrap();
        assert!(req2.body_bytes.is_none());
    }

    #[test]
    fn http_request_body_bytes_some_roundtrips() {
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "https://example.com/upload".into(),
            headers: vec![],
            body: None,
            body_bytes: Some(vec![0x00, 0xFF, 0x10, 0x7F]),
            timeout: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        let req2: HttpRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(
            req2.body_bytes.as_deref(),
            Some(&[0x00u8, 0xFF, 0x10, 0x7F][..])
        );
    }

    #[test]
    fn http_response_body_bytes_empty_omitted_from_json() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![],
            body: "{}".into(),
            body_bytes: Vec::new(),
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            !s.contains("body_bytes"),
            "empty body_bytes must be omitted for backward-compatible shape; got: {s}"
        );
        // Legacy JSON without the field still deserializes (serde default).
        let legacy = r#"{"status":200,"headers":[],"body":"{}"}"#;
        let resp2: HttpResponse = serde_json::from_str(legacy).unwrap();
        assert!(resp2.body_bytes.is_empty());
    }

    #[test]
    fn http_response_body_bytes_preserves_invalid_utf8() {
        // A genuinely-binary body: `0xFF 0xFE` are invalid UTF-8, so the lossy
        // `body` String differs byte-for-byte from the raw wire bytes.
        let raw = vec![0x25, 0x50, 0x44, 0x46, 0x00, 0xFF, 0xFE];
        let resp = HttpResponse {
            status: 200,
            headers: vec![],
            body: String::from_utf8_lossy(&raw).into_owned(),
            body_bytes: raw.clone(),
        };
        // The lossy String is NOT byte-identical to the wire (proves the field
        // is load-bearing, not merely a copy of `body`).
        assert_ne!(resp.body.as_bytes(), raw.as_slice());
        let s = serde_json::to_string(&resp).unwrap();
        let resp2: HttpResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(resp2.body_bytes, raw, "raw bytes must survive round-trip");
    }
}
