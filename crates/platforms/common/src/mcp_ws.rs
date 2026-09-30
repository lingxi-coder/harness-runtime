//! WebSocket MCP transport. Constructs a `jsonrpc::Connection` over a
//! `tokio_tungstenite::WebSocketStream`, sending the
//! `X-LingXi-Ide-Authorization: <token>` header and the
//! `Sec-WebSocket-Protocol: mcp` subprotocol literally — both verified
//! against `claude-code/src/services/mcp/client.ts:713,722,771,446`.

use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use http::Request;
use jsonrpc::messages::Message as JsonRpcMessage;
use jsonrpc::{Connection, ConnectionError};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use url::Url;

/// Errors from [`connect_ws`].
#[derive(Debug, thiserror::Error)]
pub enum WsConnectError {
    /// Failed to build the handshake request (invalid URL, header, etc.).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Handshake failed (TCP refused, TLS error, server rejected upgrade, etc.).
    #[error("handshake: {0}")]
    Handshake(String),
}

/// LITERAL header name claude-code uses for IDE-lockfile-derived MCP auth.
/// Source: `claude-code/src/services/mcp/client.ts:713`.
pub const AUTH_HEADER_NAME: &str = "X-LingXi-Ide-Authorization";

/// LITERAL WebSocket subprotocol — single value `mcp`.
/// Source: `claude-code/src/services/mcp/client.ts:722,771,446` (`protocols: ['mcp']`).
pub const WS_SUBPROTOCOL: &str = "mcp";

/// Build the handshake `Request` carrying both auth header and subprotocol.
///
/// Public for unit testing the wire format independently of `connect_async`.
///
/// # Errors
///
/// Returns [`WsConnectError::InvalidRequest`] if the URL or header value
/// cannot be expressed in HTTP form (e.g. non-ASCII in the auth token).
pub fn build_handshake_request(url: &Url, auth_token: &str) -> Result<Request<()>, WsConnectError> {
    build_handshake_request_optional(url, Some(auth_token))
}

/// Build a WebSocket handshake with an optional local IDE auth token.
///
/// A lockfile-discovered IDE always supplies a token, while plugin-configured
/// `ws-ide` entries may omit one. In the latter case no auth header is sent.
pub fn build_handshake_request_optional(
    url: &Url,
    auth_token: Option<&str>,
) -> Result<Request<()>, WsConnectError> {
    // `tokio-tungstenite` knows how to fill all required handshake headers
    // (Sec-WebSocket-Key, Upgrade, Connection, etc.) from a `Url`. We start
    // from that baseline and then add our two custom headers.
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| WsConnectError::InvalidRequest(e.to_string()))?;
    let headers = req.headers_mut();
    if let Some(auth_token) = auth_token {
        headers.insert(
            AUTH_HEADER_NAME,
            auth_token
                .parse()
                .map_err(|e: http::header::InvalidHeaderValue| {
                    WsConnectError::InvalidRequest(e.to_string())
                })?,
        );
    }
    headers.insert(
        "Sec-WebSocket-Protocol",
        WS_SUBPROTOCOL
            .parse()
            .map_err(|e: http::header::InvalidHeaderValue| {
                WsConnectError::InvalidRequest(e.to_string())
            })?,
    );
    Ok(req)
}

/// Connect to an MCP server over WebSocket and return a wired `Connection`.
///
/// - Sends `X-LingXi-Ide-Authorization: <auth_token>` in the handshake.
/// - Negotiates the `mcp` subprotocol.
/// - Adapts WS text frames to and from `jsonrpc::Message` (one JSON
///   object per text frame). Binary frames are rejected; ping/pong are
///   handled automatically by `tokio-tungstenite`; close frames terminate
///   the inbound stream so the broker shuts down naturally.
///
/// # Errors
///
/// - [`WsConnectError::InvalidRequest`] if the handshake request cannot be
///   constructed from `url` and `auth_token`.
/// - [`WsConnectError::Handshake`] if the TCP, TLS, or HTTP-upgrade handshake
///   fails (refused, unreachable, server rejected upgrade, etc.).
pub async fn connect_ws(url: Url, auth_token: &str) -> Result<Connection, WsConnectError> {
    connect_ws_optional(url, Some(auth_token)).await
}

/// Connect to an MCP WebSocket with an optional local IDE auth token.
pub async fn connect_ws_optional(
    url: Url,
    auth_token: Option<&str>,
) -> Result<Connection, WsConnectError> {
    let req = build_handshake_request_optional(&url, auth_token)?;
    let (ws_stream, _resp) = connect_async(req)
        .await
        .map_err(|e| WsConnectError::Handshake(e.to_string()))?;

    let (ws_sink, ws_stream) = ws_stream.split();

    // Adapt: WebSocket text frame -> JsonRpcMessage. Decode errors and
    // unexpected binary frames are logged and dropped — the broker only
    // accepts `Message` items so we cannot surface a per-frame parse error.
    let inbound = ws_stream.filter_map(|item| async move {
        match item {
            Ok(Message::Text(s)) => match serde_json::from_str::<JsonRpcMessage>(&s) {
                Ok(msg) => Some(msg),
                Err(e) => {
                    tracing::warn!(error = %e, "mcp ws: malformed JSON in text frame");
                    None
                }
            },
            Ok(Message::Binary(_)) => {
                tracing::warn!("mcp ws: binary frame received (unsupported); dropping");
                None
            }
            // Close terminates the inbound stream; Ping/Pong/Frame are
            // handled internally by tokio-tungstenite and surfaced as `Ok(_)`.
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "mcp ws: transport error on inbound frame");
                None
            }
        }
    });

    // Adapt: JsonRpcMessage -> WebSocket text frame.
    let outbound = ws_sink.with(|msg: JsonRpcMessage| async move {
        let text = serde_json::to_string(&msg).map_err(|e| {
            tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;
        Ok::<_, tokio_tungstenite::tungstenite::Error>(Message::Text(text))
    });
    // Re-map tungstenite errors to ConnectionError so the Sink<Message,
    // Error = ConnectionError> bound on `from_message_streams` is satisfied.
    let outbound = outbound.sink_map_err(|e: tokio_tungstenite::tungstenite::Error| {
        ConnectionError::Broker(jsonrpc::BrokerError::Join(format!("ws sink: {e}")))
    });

    Ok(Connection::from_message_streams(
        Box::pin(inbound),
        Box::pin(outbound),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_sets_authorization_header_literal() {
        let req = build_handshake_request(
            &url::Url::parse("ws://127.0.0.1:9876/mcp").unwrap(),
            "test-token-abc",
        )
        .expect("request build");

        let header = req
            .headers()
            .get("X-LingXi-Ide-Authorization")
            .expect("X-LingXi-Ide-Authorization header missing");
        assert_eq!(header.to_str().unwrap(), "test-token-abc");
        // No `Bearer ` prefix.
        assert!(!header.to_str().unwrap().starts_with("Bearer"));
    }

    #[test]
    fn build_request_omits_optional_auth_header_when_token_is_absent() {
        let req = build_handshake_request_optional(
            &url::Url::parse("ws://127.0.0.1:9876").unwrap(),
            None,
        )
        .expect("request build");
        assert!(req.headers().get(AUTH_HEADER_NAME).is_none());
        assert_eq!(
            req.headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok()),
            Some("mcp")
        );
    }

    #[test]
    fn build_request_sets_mcp_subprotocol_literal() {
        let req =
            build_handshake_request(&url::Url::parse("ws://127.0.0.1:9876/mcp").unwrap(), "tok")
                .expect("request build");

        let proto = req
            .headers()
            .get("Sec-WebSocket-Protocol")
            .expect("Sec-WebSocket-Protocol header missing");
        assert_eq!(proto.to_str().unwrap(), "mcp");
    }

    #[test]
    fn build_request_uri_matches_url() {
        let req = build_handshake_request(
            &url::Url::parse("ws://example.com:8000/path?x=1").unwrap(),
            "tok",
        )
        .expect("request build");
        assert_eq!(req.uri().to_string(), "ws://example.com:8000/path?x=1");
    }
}
