//! MCP Streamable HTTP transport — POST request, JSON or text/event-stream response.
//! Matches claude-code's `StreamableHTTPClientTransport` from
//! `src/services/mcp/client.ts:784-901` (Accept: application/json, text/event-stream).
//!
//! Wire contract (LITERAL):
//! - POST `Content-Type: application/json` body is a single JSON-RPC frame.
//! - `Accept: application/json, text/event-stream` (the literal claude-code
//!   `MCP_STREAMABLE_HTTP_ACCEPT` const from `client.ts:471`).
//! - LingXi product/version `User-Agent`, with the native MCP CLI shape.
//! - When `auth_token` is `Some`, POST carries
//!   `X-LingXi-Ide-Authorization: <token>` verbatim (no `Bearer ` prefix).
//! - Response body is EITHER a single `application/json` frame OR a
//!   `text/event-stream` body of zero-or-more frames; content-type selects.

use futures::StreamExt;
use jsonrpc::messages::Message as JsonRpcMessage;
use jsonrpc::{BrokerError, Connection, ConnectionError};
use lingxi_core::host::mcp::McpError;
use lingxi_core::host::mcp_result::CancellationToken;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

use lingxi_llm_client::transport::http_backend as reqwest;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::mpsc;

struct HttpCleanupCompletion(CancellationToken);
impl Drop for HttpCleanupCompletion {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[path = "mcp_http_body.rs"]
pub(crate) mod body_budget;
#[path = "mcp_sse_codec.rs"]
pub(crate) mod sse_codec;

#[derive(Debug)]
pub(crate) struct BodyReadError {
    pub diagnostic: String,
    pub limit: Option<usize>,
}

pub(crate) async fn read_native_body(
    response: reqwest::Response,
) -> Result<Vec<u8>, BodyReadError> {
    let mut budget =
        body_budget::Budget::new(body_budget::MESSAGE_BYTES, Some(body_budget::MESSAGE_BYTES));
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| BodyReadError {
            diagnostic: error.to_string(),
            limit: None,
        })?;
        budget.push(&chunk).map_err(|error| BodyReadError {
            diagnostic: error.diagnostic(),
            limit: Some(error.limit),
        })?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Clone)]
struct HttpStreamContext {
    client: reqwest::Client,
    url: String,
    auth: Option<String>,
    extra: lingxi_core::host::McpHeaders,
    session_id: Arc<Mutex<Option<String>>>,
    protocol_version: Arc<Mutex<Option<String>>>,
    inbound: mpsc::UnboundedSender<JsonRpcMessage>,
    fatal: mpsc::UnboundedSender<()>,
    retry_ms: Arc<Mutex<Option<u64>>>,
    active: Arc<Mutex<HashMap<jsonrpc::Id, (JsonRpcMessage, CancellationToken)>>>,
    abort: CancellationToken,
}

impl HttpStreamContext {
    fn register(&self, frame: &JsonRpcMessage, cancellation: CancellationToken) {
        let JsonRpcMessage::Request(request) = frame else {
            return;
        };
        let id = request.id.clone();
        let completion = CancellationToken::new();
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), (frame.clone(), completion.clone()));
        let active = self.active.clone();
        let abort = self.abort.clone();
        tokio::spawn(async move {
            tokio::select! {biased; _=completion.cancelled()=>{}, _=abort.cancelled()=>{}, _=cancellation.cancelled()=>{active.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);}}
        });
    }

    fn forward(
        &self,
        message: JsonRpcMessage,
    ) -> Result<(), mpsc::error::SendError<JsonRpcMessage>> {
        if let JsonRpcMessage::Response(response) = &message {
            if let Some(id) = &response.id {
                if let Some((_, completion)) = self
                    .active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id)
                {
                    completion.cancel();
                }
            }
        }
        self.inbound.send(message)
    }

    fn syntax_failure(&self, diagnostic: &str) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(request, _)| request.clone())
            .collect::<Vec<_>>();
        for request in active {
            let error = probe_compatibility_error(&request, "malformed discovery response")
                .or_else(|| native_frame_error(&request, "syntax", diagnostic));
            if let Some(error) = error {
                let _ = self.forward(error);
            }
        }
        let _ = self.fatal.send(());
    }
    fn headers(&self, cursor: Option<&str>) -> Result<HeaderMap, HttpConnectError> {
        let sid = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let version = self
            .protocol_version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut headers = build_common_headers(
            self.auth.as_deref(),
            &self.extra,
            sid.as_deref(),
            version.as_deref(),
        )?;
        extend_accept(&mut headers, &["text/event-stream"]);
        if let Some(cursor) = cursor.filter(|value| !value.is_empty()) {
            headers.insert(
                HeaderName::from_static("last-event-id"),
                HeaderValue::from_str(cursor)
                    .map_err(|error| HttpConnectError::Transport(error.to_string()))?,
            );
        }
        Ok(headers)
    }

    fn delay(&self, attempt: u32) -> std::time::Duration {
        let configured = *self
            .retry_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::time::Duration::from_millis(
            configured.unwrap_or_else(|| {
                (1_000.0_f64 * 1.5_f64.powi(attempt as i32)).min(30_000.0) as u64
            }),
        )
    }
}

#[derive(Default)]
struct HttpStreamEnd {
    cursor: Option<String>,
    answered: bool,
    fatal: bool,
}

pub(crate) fn frame_diagnostic(error: &jsonrpc::CodecError) -> String {
    match error {
        jsonrpc::CodecError::MessageDecode(detail) => detail.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn native_frame_error(
    request: &JsonRpcMessage,
    kind: &str,
    diagnostic: &str,
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?;
    serde_json::from_value(serde_json::json!({
        "jsonrpc":"2.0", "id":id,
        "error":{"code":-32001,"message":diagnostic,"data":{"nativeFailure":kind,"diagnostic":diagnostic}}
    })).ok()
}

async fn read_http_sse(
    context: &HttpStreamContext,
    response: reqwest::Response,
    request: Option<&JsonRpcMessage>,
    is_get: bool,
) -> HttpStreamEnd {
    let mut state = HttpStreamEnd::default();
    let mut decoder = sse_codec::Decoder::default();
    let streaming = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(body_budget::is_event_stream);
    let subscription = request.is_some_and(|request| match request {
        JsonRpcMessage::Request(request) => {
            request.method == "subscriptions/listen"
                && request.params.as_ref().is_some_and(|params| {
                    params
                        .as_json()
                        .contains("io.modelcontextprotocol/protocolVersion")
                })
        }
        _ => false,
    });
    let total = if streaming && response.status().as_u16() == 200 && (is_get || subscription) {
        None
    } else {
        Some(if streaming {
            body_budget::MESSAGE_BYTES * 4
        } else {
            body_budget::MESSAGE_BYTES
        })
    };
    let mut budget = body_budget::Budget::new(body_budget::MESSAGE_BYTES, total);
    let mut bytes = response.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                tracing::warn!(%error, "mcp http: SSE stream disconnected");
                break;
            }
        };
        if let Err(error) = budget.push(&chunk) {
            context.syntax_failure(&error.diagnostic());
            state.fatal = true;
            return state;
        }
        for item in decoder.push(&chunk) {
            let event = match item {
                sse_codec::Item::Retry(value) => {
                    *context
                        .retry_ms
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
                    continue;
                }
                sse_codec::Item::Event(event) => event,
            };
            if let Some(id) = event.id.filter(|id| !id.is_empty()) {
                state.cursor = Some(id);
            }
            if event.data.is_empty() || !is_message_event(&event.event) {
                continue;
            }
            match mcp::decode_native_mcp_frame(event.data.as_bytes()) {
                Ok(message) => {
                    if let Some(request) = request {
                        observe_protocol_response(
                            &serde_json::to_value(request).unwrap_or_default(),
                            &message,
                            &mut context
                                .protocol_version
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        );
                    }
                    state.answered |= matches!(&message, JsonRpcMessage::Response(_));
                    if context.forward(message).is_err() {
                        state.fatal = true;
                        return state;
                    }
                }
                Err(error) => {
                    // HTTP/SSE SyntaxError closes the whole native transport;
                    // schema errors are reported but a subsequent event can be valid.
                    let syntax = Utf16JsonProjection::parse(&event.data).is_err();
                    tracing::warn!(%error, syntax, "mcp http: invalid SSE message");
                    if syntax {
                        context.syntax_failure(&frame_diagnostic(&error));
                        state.fatal = true;
                        return state;
                    }
                }
            }
        }
    }
    state
}

/// Continue only an established stream. A request-bearing POST must advertise
/// an event id before GET replay is possible; notification GET streams can
/// reconnect without one. A successful attachment resets the failed-attempt
/// budget, while two failed GET attempts leave the POST transport usable.
async fn continue_http_stream(
    context: HttpStreamContext,
    response: Option<reqwest::Response>,
    request: Option<JsonRpcMessage>,
) {
    let mut cursor = None;
    let mut reconnect = false;
    let mut next_response = response.map(|response| (response, false));
    loop {
        if let Some((response, is_get)) = next_response.take() {
            let ended = read_http_sse(&context, response, request.as_ref(), is_get).await;
            if ended.fatal || ended.answered {
                return;
            }
            if ended.cursor.is_some() {
                cursor = ended.cursor;
            }
            if request.is_some() && cursor.is_none() {
                // A closed, non-resumable stream does not fabricate an RPC
                // response. Native leaves the SDK request's timer in charge.
                return;
            }
            reconnect = true;
        }
        let mut attempt = 0;
        loop {
            if reconnect {
                tokio::time::sleep(context.delay(attempt)).await;
            }
            let headers = match context.headers(cursor.as_deref()) {
                Ok(headers) => headers,
                Err(error) => {
                    tracing::warn!(%error, "mcp http: invalid replay headers");
                    return;
                }
            };
            let response = context
                .client
                .get(&context.url)
                .headers(headers)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    next_response = Some((response, true));
                    break;
                }
                Ok(response) if response.status().as_u16() == 405 => return,
                response => {
                    tracing::warn!(?response, attempt, "mcp http: failed to open SSE stream");
                    if !reconnect || attempt >= 1 {
                        return;
                    }
                    attempt += 1;
                }
            }
        }
    }
}

async fn receive_notifications(context: HttpStreamContext, cancel: CancellationToken) {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {},
        _ = context.inbound.closed() => {},
        _ = continue_http_stream(context.clone(), None, None) => {},
    }
}

/// Only router-assigned ids can register a POST token. The writer takes each
/// token exactly once; cancelled queued tokens prevent delayed network writes.
#[derive(Default)]
pub(crate) struct HttpRequestCancellation {
    pending: Mutex<HashMap<jsonrpc::Id, CancellationToken>>,
}
impl jsonrpc::router::PerRequestCancellation for HttpRequestCancellation {
    fn prepare(&self, id: &jsonrpc::Id) -> CancellationToken {
        let token = CancellationToken::new();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), token.clone());
        token
    }
    fn discard(&self, id: &jsonrpc::Id) {
        if let Some(token) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            token.cancel();
        }
    }
    fn close(&self) {
        for (_, token) in self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
        {
            token.cancel();
        }
    }
}
impl HttpRequestCancellation {
    pub(crate) fn take(&self, frame: &JsonRpcMessage) -> CancellationToken {
        match frame {
            JsonRpcMessage::Request(request) => self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request.id)
                .unwrap_or_else(|| {
                    let token = CancellationToken::new();
                    token.cancel();
                    token
                }),
            _ => CancellationToken::new(),
        }
    }
}

/// Literal `Accept` header value for Streamable HTTP. Matches claude-code's
/// `MCP_STREAMABLE_HTTP_ACCEPT` const (`client.ts:471`) byte-for-byte.
const STREAMABLE_HTTP_ACCEPT: &[&str] = &["application/json", "text/event-stream"];

/// Product identity follows LingXi while retaining the MCP CLI client shape.
#[must_use]
pub fn user_agent() -> String {
    format!("lingxi/{} (sdk-cli)", env!("CARGO_PKG_VERSION"))
}

/// HTTP session lifetime is independent from a connection's stream lifetime.
/// Closing a connection aborts readers; only explicit termination sends DELETE.
#[derive(Clone)]
pub struct HttpSessionControl {
    client: reqwest::Client,
    url: String,
    auth: Option<String>,
    extra: lingxi_core::host::McpHeaders,
    session_id: Arc<Mutex<Option<String>>>,
    protocol_version: Arc<Mutex<Option<String>>>,
    closed: CancellationToken,
    abort: CancellationToken,
}

impl HttpSessionControl {
    /// Wait until all owned request and notification readers have stopped.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    /// Whether connection close has completed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Explicitly terminate the current server session without closing streams.
    /// Successful responses and HTTP 405 clear the session; other failures retain
    /// it so callers can inspect the error and retry, matching the native API.
    pub async fn terminate_session(&self) -> Result<(), HttpConnectError> {
        let session = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(session) = session else {
            return Ok(());
        };
        let version = self
            .protocol_version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let headers = build_common_headers(
            self.auth.as_deref(),
            &self.extra,
            Some(&session),
            version.as_deref(),
        )?;
        let send = self.client.delete(&self.url).headers(headers).send();
        let response = tokio::select! {
            biased;
            _ = self.abort.cancelled() => return Err(HttpConnectError::Transport("AbortError: The operation was aborted.".into())),
            response = send => response.map_err(|error| HttpConnectError::Transport(error.to_string()))?,
        };
        let status = response.status();
        let authenticate = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        tokio::select! { biased; _=self.abort.cancelled()=>{}, _=read_native_body(response)=>{} }
        if !status.is_success() && status.as_u16() != 405 {
            return Err(HttpConnectError::HttpResponse {
                status: status.as_u16(),
                www_authenticate: authenticate,
            });
        }
        *self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

/// Errors specific to opening an MCP Streamable HTTP connection.
#[derive(Debug, Error)]
pub enum HttpConnectError {
    /// HTTP request setup failed.
    #[error("http transport error: {0}")]
    Transport(String),
    /// Authorization header value was invalid (non-ASCII, control chars).
    #[error("invalid auth token: {0}")]
    InvalidAuth(String),
    /// Remote response retained for authentication recovery.
    #[error("HTTP {status}{detail}", detail = www_authenticate.as_ref().map(|value| format!(": {value}")).unwrap_or_default())]
    HttpResponse {
        /// HTTP status code.
        status: u16,
        /// `WWW-Authenticate` response header.
        www_authenticate: Option<String>,
    },
}

impl From<HttpConnectError> for McpError {
    fn from(value: HttpConnectError) -> Self {
        match value {
            HttpConnectError::HttpResponse {
                status,
                www_authenticate,
            } => Self::HttpResponse {
                status,
                www_authenticate,
            },
            other => Self::Connection(other.to_string().into()),
        }
    }
}

/// Native's HTTP response guard first parses Content-Type, then falls back to
/// its trimmed essence for malformed parameters unless the remainder contains
/// a comma. This intentionally differs from legacy EventSource's prefix test.
pub(crate) fn content_type_essence(value: &str) -> Option<String> {
    let (raw_type, parameters) = value.split_once(';').unwrap_or((value, ""));
    let essence = raw_type.trim_matches(js_whitespace).to_ascii_lowercase();
    if essence.is_empty() {
        return None;
    }
    let valid_type = essence.split_once('/').is_some_and(|(top, sub)| {
        !top.is_empty()
            && !sub.is_empty()
            && top.bytes().all(mime_token_byte)
            && sub.bytes().all(mime_token_byte)
    });
    if valid_type
        && (parameters.is_empty()
            || parameters
                .chars()
                .all(|character| u32::from(character) <= 255)
                && valid_mime_parameters(parameters.as_bytes()))
    {
        return Some(essence);
    }
    let start = essence.encode_utf16().count();
    if value
        .encode_utf16()
        .skip(start)
        .any(|unit| unit == u16::from(b','))
    {
        None
    } else {
        Some(essence)
    }
}

fn mime_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn valid_mime_parameters(mut input: &[u8]) -> bool {
    loop {
        while input.first() == Some(&b' ') {
            input = &input[1..];
        }
        let name_length = input
            .iter()
            .take_while(|byte| mime_token_byte(**byte))
            .count();
        if name_length == 0 {
            return false;
        }
        input = &input[name_length..];
        while input.first() == Some(&b' ') {
            input = &input[1..];
        }
        if input.first() != Some(&b'=') {
            return false;
        }
        input = &input[1..];
        while input.first() == Some(&b' ') {
            input = &input[1..];
        }
        if input.first() == Some(&b'"') {
            input = &input[1..];
            loop {
                let Some((&byte, rest)) = input.split_first() else {
                    return false;
                };
                input = rest;
                if byte == b'"' {
                    break;
                }
                if byte == b'\\' {
                    let Some((&escaped, rest)) = input.split_first() else {
                        return false;
                    };
                    if escaped != 11 && escaped < 32 {
                        return false;
                    }
                    input = rest;
                } else if byte != 11 && (byte < 32 || byte == 127) {
                    return false;
                }
            }
        } else {
            let length = input
                .iter()
                .take_while(|byte| mime_token_byte(**byte))
                .count();
            if length == 0 {
                return false;
            }
            input = &input[length..];
        }
        while input.first() == Some(&b' ') {
            input = &input[1..];
        }
        if input.is_empty() {
            return true;
        }
        if input.first() != Some(&b';') {
            return false;
        }
        input = &input[1..];
    }
}

pub(crate) fn is_message_event(event: &str) -> bool {
    event.is_empty() || event == "message"
}

fn decode_http_json_frames(body: &[u8]) -> Result<Vec<JsonRpcMessage>, jsonrpc::CodecError> {
    let source = String::from_utf8_lossy(body);
    let projection = Utf16JsonProjection::parse(&source)
        .map_err(|error| jsonrpc::CodecError::MessageDecode(error.to_string()))?;
    let Some(length) = projection.array_len("") else {
        return mcp::decode_native_mcp_frame(body).map(|message| vec![message]);
    };
    (0..length)
        .map(|index| {
            let item = projection
                .subprojection(&format!("/{index}"))
                .and_then(|item| item.to_json_string())
                .map_err(|error| jsonrpc::CodecError::MessageDecode(error.to_string()))?;
            mcp::decode_native_mcp_frame(item.as_bytes())
        })
        .collect()
}

pub(crate) fn build_common_headers<H>(
    auth_token: Option<&str>,
    extra_headers: &H,
    session_id: Option<&str>,
    protocol_version: Option<&str>,
) -> Result<HeaderMap, HttpConnectError>
where
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let mut entries = vec![
        ("User-Agent".to_owned(), user_agent()),
        ("Accept-Encoding".to_owned(), "identity".to_owned()),
    ];
    if let Some(token) = auth_token {
        let _ = HeaderValue::try_from(token)
            .map_err(|e| HttpConnectError::InvalidAuth(e.to_string()))?;
        entries.push(("X-LingXi-Ide-Authorization".to_owned(), token.to_owned()));
    }
    for (name, value) in [
        ("mcp-session-id", session_id),
        ("mcp-protocol-version", protocol_version),
    ] {
        if let Some(value) = value {
            entries.push((name.to_owned(), value.to_owned()));
        }
    }
    for (k, v) in extra_headers {
        // Object spread replaces an exactly-spelled property, but distinct
        // spellings survive until Headers joins their case-insensitive names.
        if let Some((_, value)) = entries.iter_mut().find(|(name, _)| name == k) {
            value.clone_from(v);
        } else {
            entries.push((k.clone(), v.clone()));
        }
    }
    let mut h = HeaderMap::new();
    for (raw_name, raw_value) in entries {
        let name = HeaderName::try_from(raw_name.as_str()).map_err(|error| {
            HttpConnectError::Transport(format!("bad header name {raw_name}: {error}"))
        })?;
        let value = raw_value.trim_matches([' ', '\t', '\r', '\n']);
        let value = match h
            .get(&name)
            .and_then(|value: &HeaderValue| value.to_str().ok())
        {
            Some(existing) => format!("{existing}, {value}"),
            None => value.to_owned(),
        };
        h.insert(
            name,
            HeaderValue::from_str(&value).map_err(|error| {
                HttpConnectError::Transport(format!("bad header value: {error}"))
            })?,
        );
    }
    Ok(h)
}

pub(crate) fn extend_accept(headers: &mut HeaderMap, required: &[&str]) {
    let mut values = Vec::new();
    if let Some(accept) = headers.get(ACCEPT).and_then(|value| value.to_str().ok()) {
        for value in accept.split(',') {
            let value = value.trim().to_ascii_lowercase();
            if !values.contains(&value) {
                values.push(value);
            }
        }
    }
    for value in required {
        if !values.iter().any(|existing| existing == value) {
            values.push((*value).to_owned());
        }
    }
    headers.insert(
        ACCEPT,
        HeaderValue::from_str(&values.join(", ")).expect("validated Accept values remain valid"),
    );
}

fn build_headers<H>(
    auth_token: Option<&str>,
    extra_headers: &H,
    session_id: Option<&str>,
    protocol_version: Option<&str>,
) -> Result<HeaderMap, HttpConnectError>
where
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let mut headers =
        build_common_headers(auth_token, extra_headers, session_id, protocol_version)?;
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    extend_accept(&mut headers, STREAMABLE_HTTP_ACCEPT);
    Ok(headers)
}

/// Open an MCP Streamable HTTP connection.
///
/// Each outbound JSON-RPC frame is `POSTed` to `url`. The server may reply with
/// either a single `application/json` body OR a `text/event-stream` body of
/// zero-or-more frames — both modes are decoded and routed back through
/// `jsonrpc::Connection`.
///
/// `extra_headers` is generic over the map type so both an unordered `HashMap`
/// and the insertion-ordered [`lingxi_core::host::McpHeaders`] (`IndexMap`) the MCP
/// transport specs now carry are accepted (header order is irrelevant to the
/// emitted HTTP request).
///
/// `fetch_timeout` bounds the time-to-response-*headers* of each outbound POST
/// (Claude Code's fetch wrapper — fetch resolves once headers arrive and the
/// timer is cleared, so a streaming `text/event-stream` body is read afterwards
/// WITHOUT this bound). `None` disables the bound (byte-identical to the prior
/// behavior). Callers pass `mcp::client::mcp_http_fetch_timeout_for(..)` (default
/// `60_000`ms). A POST that exceeds it fails its pending call through the local
/// transport-error mapping, without closing the connection.
///
/// # Errors
///
/// - [`HttpConnectError::Transport`] if reqwest client construction fails.
/// - [`HttpConnectError::InvalidAuth`] if `auth_token` contains bytes that
///   cannot be expressed in an HTTP header value (non-ASCII, control chars).
pub async fn connect_http<H>(
    url: &str,
    auth_token: Option<&str>,
    extra_headers: &H,
    fetch_timeout: Option<std::time::Duration>,
) -> Result<Connection, HttpConnectError>
where
    H: Clone + Send + 'static,
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    connect_http_with_proxy_auth_helper(url, auth_token, extra_headers, fetch_timeout, None)
        .await
        .map(|(connection, _)| connection)
}

/// Open an HTTP connection with explicit server-session termination authority.
pub async fn connect_http_with_session<H>(
    url: &str,
    auth_token: Option<&str>,
    extra_headers: &H,
    fetch_timeout: Option<std::time::Duration>,
) -> Result<(Connection, HttpSessionControl), HttpConnectError>
where
    H: Clone + Send + 'static,
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    connect_http_with_proxy_auth_helper(url, auth_token, extra_headers, fetch_timeout, None).await
}

pub(crate) async fn connect_http_with_proxy_auth_helper<H>(
    url: &str,
    auth_token: Option<&str>,
    extra_headers: &H,
    fetch_timeout: Option<std::time::Duration>,
    proxy_auth_helper: Option<Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>>,
) -> Result<(Connection, HttpSessionControl), HttpConnectError>
where
    H: Clone + Send + 'static,
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let client = crate::mcp_proxy::build_client_with_proxy_auth_helper(url, proxy_auth_helper)
        .map_err(|e| HttpConnectError::Transport(e.to_string()))?;

    // Validate header construction eagerly so configuration errors surface at
    // connect-time rather than on the first POST.
    let _ = build_headers(auth_token, extra_headers, None, None)?;

    // Inbound: frames produced by the POST writer task (from JSON or SSE
    // response bodies). Outbound: frames the Connection wants to POST.
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

    let post_url = url.to_string();
    let post_auth = auth_token.map(str::to_string);
    let post_extra: lingxi_core::host::McpHeaders = extra_headers
        .into_iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let post_fetch_timeout = fetch_timeout;

    // MCP Streamable HTTP session ID: captured from the `mcp-session-id`
    // response header on the initialize response and included as
    // `Mcp-Session-Id` in every subsequent request (1:1 with claude-code's
    // `StreamableHTTPClientTransport` session tracking).
    let session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let writer_sid = Arc::clone(&session_id);

    let request_cancellation = Arc::new(HttpRequestCancellation::default());
    let writer_request_cancellation = request_cancellation.clone();
    let cleanup_done = CancellationToken::new();
    let writer_cleanup_done = cleanup_done.clone();
    let connection_abort = CancellationToken::new();
    let writer_abort = connection_abort.clone();
    let protocol_version = Arc::new(Mutex::new(None::<String>));
    let session_control = HttpSessionControl {
        client: client.clone(),
        url: post_url.clone(),
        auth: post_auth.clone(),
        extra: post_extra.clone(),
        session_id: session_id.clone(),
        protocol_version: protocol_version.clone(),
        closed: cleanup_done,
        abort: connection_abort,
    };
    tokio::spawn(async move {
        let _completion = HttpCleanupCompletion(writer_cleanup_done);
        // A POST owns its response body independently: input-required retries
        // and ordinary calls can proceed while an earlier SSE stream stays open.
        // The connection owns and drains every reader on close.
        let mut posts = tokio::task::JoinSet::new();
        let (initialized_tx, mut initialized_rx) = mpsc::unbounded_channel::<()>();
        let (stream_fault_tx, mut stream_fault_rx) = mpsc::unbounded_channel::<()>();
        let notification_cancel = CancellationToken::new();
        let stream_context = HttpStreamContext {
            client: client.clone(),
            url: post_url.clone(),
            auth: post_auth.clone(),
            extra: post_extra.clone(),
            session_id: writer_sid.clone(),
            protocol_version: protocol_version.clone(),
            inbound: inbound_tx.clone(),
            fatal: stream_fault_tx.clone(),
            retry_ms: Arc::new(Mutex::new(None)),
            active: Arc::new(Mutex::new(HashMap::new())),
            abort: writer_abort.clone(),
        };
        let mut notification_reader = None;
        loop {
            tokio::select! {
                biased;
                _=inbound_tx.closed()=>break,
                _=stream_fault_rx.recv()=>break,
                _=initialized_rx.recv(), if notification_reader.is_none()=>{
                    notification_reader = Some(tokio::spawn(receive_notifications(stream_context.clone(), notification_cancel.clone())));
                }
                settled=posts.join_next(), if !posts.is_empty()=>{
                    if let Some(Err(error))=settled {tracing::warn!(%error,"mcp http: POST task failed");}
                }
                frame=outbound_rx.recv()=>{
                    let Some(frame)=frame else {break;};
                    let request_abort=writer_request_cancellation.take(&frame);
                    if request_abort.is_cancelled() {continue;}
                    stream_context.register(&frame,request_abort.clone());
                    let client=client.clone();let post_url=post_url.clone();let post_auth=post_auth.clone();let post_extra=post_extra.clone();let writer_sid=writer_sid.clone();let writer_protocol_version=protocol_version.clone();let initialized_tx=initialized_tx.clone();let stream_context=stream_context.clone();
                    posts.spawn(async move {
                        let process_frame=async move {
                    let frame_value = serde_json::to_value(&frame).unwrap_or_default();
                    let is_probe = frame_value["method"] == "server/discover";
                    let mut headers = {
                        let sid = writer_sid.lock().unwrap();
                        let version = writer_protocol_version.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match build_headers::<lingxi_core::host::McpHeaders>(post_auth.as_deref(), &post_extra, sid.as_deref(), version.as_deref()) {
                            Ok(h) => h,
                            Err(e) => {
                                tracing::error!(error = %e, "mcp http: failed to build POST headers");
                                return;
                            }
                        }
                    };

                    if frame_value["method"] == "initialize" {
                        headers.remove("mcp-session-id");
                    }

                    apply_body_derived_headers(&mut headers, &frame_value);

                    // The native fetch timeout bounds only the time-to-response
                    // (`.send()` resolves on headers, like `await fetch(...)`); once we
                    // hold the response the streaming SSE body is read without this bound.
                    let send_fut = client.post(&post_url).headers(headers).json(&frame).send();
                    let sent = match post_fetch_timeout {
                        Some(t) => match tokio::time::timeout(t, send_fut).await {
                            Ok(r) => r,
                            Err(_) => {
                                tracing::warn!(
                                    "mcp http: POST timed out awaiting response headers"
                                );
                                if let Some(error) = outbound_transport_error(
                                    &frame,
                                    "POST timed out awaiting response headers",
                                ) {
                                    let _ = stream_context.forward(error);
                                }
                                return;
                            }
                        },
                        None => send_fut.await,
                    };
                    let response = match sent {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "mcp http: POST failed");
                            if let Some(error) = outbound_transport_error(&frame, &e.to_string()) {
                                let _ = stream_context.forward(error);
                            }
                            return;
                        }
                    };

                    // Capture MCP session ID from response headers for subsequent
                    // requests (MCP Streamable HTTP §session).
                    if let Some(sid_val) = response.headers().get("mcp-session-id") {
                        if let Ok(val) = sid_val.to_str() {
                            if !val.is_empty() {
                                *writer_sid.lock().unwrap() = Some(val.to_string());
                            }
                        }
                    }

                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let www_authenticate = response
                            .headers()
                            .get(reqwest::header::WWW_AUTHENTICATE)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        // Streamable HTTP servers commonly explain a stale session in
                        // a 400 response body (for example "Server not initialized").
                        // Preserve only a small local diagnostic prefix so the MCP
                        // client can apply the SDK's narrow stale-session classifier;
                        // this value is never attached to telemetry.
                        //
                        // 404 and 405 are kept for a second reader: they are how a
                        // server that speaks only legacy HTTP+SSE rejects a streamable
                        // `initialize` POST, and the legacy fallback decides whether to
                        // re-dial by asking whether that body is a JSON-RPC message. A
                        // rejection carrying a real JSON-RPC error is a protocol
                        // failure and must NOT be re-dialled, so the body is the
                        // evidence, not the status alone.
                        let body = read_native_body(response).await.map(|body|String::from_utf8_lossy(&body).into_owned()).unwrap_or_else(|_|"null".to_owned());
                        // A modern HTTP 400 may carry the request's actual
                        // JSON-RPC error, which remains a protocol response.
                        if status==400 && frame_value.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion").and_then(serde_json::Value::as_str)==Some("2026-07-28") {
                            if let Ok(JsonRpcMessage::Response(reply))=mcp::decode_native_mcp_frame(body.as_bytes()) {
                                if reply.error.is_some() && reply.id.as_ref()==match &frame{JsonRpcMessage::Request(request)=>Some(&request.id),_=>None} {
                                    let _=stream_context.forward(JsonRpcMessage::Response(reply));return;
                                }
                            }
                        }
                        tracing::warn!(status, "mcp http: non-success response");
                        if let Some(error) =
                            http_error_message(&frame, status, www_authenticate.as_deref(), &body)
                        {
                            if stream_context.forward(error).is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    if response.status().as_u16() == 202 {
                        // The native fetch wrapper cancels 202/notification bodies.
                        if frame_value["method"] == "notifications/initialized" {
                            let _ = initialized_tx.send(());
                        }
                        return;
                    }
                    // A notification/response POST has no request whose answer
                    // could be delivered. Native drains and ignores its body.
                    if !matches!(&frame, JsonRpcMessage::Request(_)) {
                        return;
                    }

                    let content_type_header = response
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let content_type = content_type_header.as_deref().and_then(content_type_essence);

                    if content_type.as_deref() == Some("text/event-stream") {
                        continue_http_stream(stream_context, Some(response), Some(frame.clone())).await;
                    } else if content_type.as_deref() == Some("application/json") {
                        // JSON may carry one message or an array. Native
                        // validates the whole array before forwarding entries.
                        match read_native_body(response).await {
                            Ok(body) => {
                                // Fetch's Response.json() strips one leading
                                // UTF-8 BOM before JSON.parse. Preserve that
                                // HTTP-only behavior; stdio/SSE/WS keep the
                                // Native decoder's direct JSON.parse policy.
                                let body = body
                                    .strip_prefix(b"\xef\xbb\xbf")
                                    .unwrap_or(body.as_ref());
                                match decode_http_json_frames(body) {
                                Ok(messages) => {
                                  for msg in messages {
                                    observe_protocol_response(
                                        &frame_value,
                                        &msg,
                                        &mut writer_protocol_version.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
                                    );
                                    let _ = stream_context.forward(msg);
                                  }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "mcp http: response not valid MCP JSON-RPC");
                                    let syntax = Utf16JsonProjection::parse(&String::from_utf8_lossy(body)).is_err();
                                    if syntax { stream_context.syntax_failure(&frame_diagnostic(&e)); }
                                    else {
                                        let error = if is_probe { probe_compatibility_error(&frame, "malformed discovery response") }
                                            else { native_frame_error(&frame, "schema", &frame_diagnostic(&e)) };
                                        if let Some(error)=error {let _=stream_context.forward(error);}
                                    }

                                }
                            }
                            },
                            Err(e) => {
                                tracing::warn!(error = %e.diagnostic, "mcp http: failed to read JSON response body");
                                let error = if is_probe { probe_compatibility_error(&frame, "malformed discovery response") }
                                    else if let Some(limit)=e.limit {
                                        let request=serde_json::to_value(&frame).unwrap_or_default();
                                        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32001,"message":e.diagnostic,"data":{"nativeFailure":"body-overflow","diagnostic":e.diagnostic,"limitBytes":limit}}})).ok()
                                    } else { outbound_transport_error(&frame, &e.diagnostic) };
                                if let Some(error) = error { let _ = stream_context.forward(error); }

                            }
                        }
                    } else if let Some(error) = outbound_transport_error(
                        &frame,
                        &format!("Unexpected content type: {}", content_type_header.as_deref().unwrap_or("null")),
                    ) {
                        let _ = stream_context.forward(error);
                    }
                        };
                        tokio::select! { biased; _=request_abort.cancelled()=>{}, _=process_frame=>{} }
                    });
                }
            }
        }
        writer_abort.cancel();
        jsonrpc::router::PerRequestCancellation::close(writer_request_cancellation.as_ref());
        posts.abort_all();
        notification_cancel.cancel();
        if let Some(reader) = notification_reader {
            let _ = reader.await;
        }
        while posts.join_next().await.is_some() {}
    });

    // Adapt mpsc channels to the Stream/Sink shape `from_message_streams` wants.
    let inbound = futures::stream::unfold(inbound_rx, |mut rx| async move {
        rx.recv().await.map(|m| (m, rx))
    });
    let outbound = futures::sink::unfold(outbound_tx, |tx, msg: JsonRpcMessage| async move {
        tx.send(msg).map_err(|_| {
            ConnectionError::Broker(BrokerError::Join("http writer task closed".into()))
        })?;
        Ok::<_, ConnectionError>(tx)
    });

    let connection = Connection::from_message_streams_with_initial_request_id(
        Box::pin(inbound),
        Box::pin(outbound),
        Some("Method not found".into()),
        0,
    );
    connection.set_per_request_cancellation(request_cancellation);
    Ok((connection, session_control))
}

fn apply_body_derived_headers(headers: &mut HeaderMap, request: &serde_json::Value) {
    // Native applies these fields to request envelopes only (`uoe`), never
    // to notifications/responses that happen to carry the same metadata.
    if !request
        .get("id")
        .is_some_and(|id| id.is_string() || id.is_number())
    {
        return;
    }
    let Some(version) = request
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(version) {
        headers.insert(HeaderName::from_static("mcp-protocol-version"), value);
    }
    if let Ok(value) = HeaderValue::from_str(method) {
        headers.insert(HeaderName::from_static("mcp-method"), value);
    }
    let name = if method == "resources/read" {
        request.pointer("/params/uri")
    } else {
        request.pointer("/params/name")
    };
    let name = if method.starts_with("tasks/") {
        request
            .pointer("/params/taskId")
            .filter(|value| value.is_string())
            .or(name)
    } else {
        name
    };
    if let Some(name) = name.and_then(serde_json::Value::as_str) {
        // Upstream Er encodes unsafe, whitespace-delimited or already wrapped
        // strings so mcp-name always round-trips through an HTTP field value.
        let needs_encoding = name.is_empty()
            || name.starts_with("=?base64?") && name.ends_with("?=")
            || name.trim_matches(js_whitespace) != name
            || name
                .bytes()
                .any(|byte| byte != b'\t' && !(32..=126).contains(&byte));
        let value = if needs_encoding {
            use base64::Engine;
            format!(
                "=?base64?{}?=",
                base64::engine::general_purpose::STANDARD.encode(name)
            )
        } else {
            name.to_string()
        };
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static("mcp-name"), value);
        }
    }
}

fn js_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

pub(crate) fn observe_protocol_response(
    request: &serde_json::Value,
    response: &JsonRpcMessage,
    protocol_version: &mut Option<String>,
) {
    let Some(request) = Utf16JsonProjection::from_serde_value(request).ok() else {
        return;
    };
    let Some(response) = serde_json::to_string(response)
        .ok()
        .and_then(|source| Utf16JsonProjection::parse(&source).ok())
    else {
        return;
    };
    let (Some(response_id), Some(request_id)) = (
        response.subprojection("/id").ok(),
        request.subprojection("/id").ok(),
    ) else {
        return;
    };
    if !response_id.semantically_eq(&request_id) {
        return;
    }
    if request.string_at("/method") == Some("initialize") {
        if let Some(version) = response.string_at("/result/protocolVersion") {
            *protocol_version = Some(version.to_string());
        }
    } else if request.string_at("/method") == Some("server/discover") {
        if let Some(discovery) = response
            .subprojection("/result")
            .ok()
            .and_then(super::mcp_remote::parse_modern_discovery)
        {
            *protocol_version = Some(discovery.version);
        }
    }
}

fn probe_error(
    request: &JsonRpcMessage,
    message: &str,
    transport_failure: bool,
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    if request["method"] != "server/discover" {
        return None;
    }
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": request.get("id")?,
        "error": { "code": -32001, "message": message, "data": {"probeTransportFailure": transport_failure} }
    })).ok()
}

fn probe_transport_error(request: &JsonRpcMessage, message: &str) -> Option<JsonRpcMessage> {
    probe_error(request, message, true)
}

/// Fail the in-flight JSON-RPC id immediately. Discovery keeps its probe
/// payload; every other request still gets an error instead of sitting out
/// the caller's full RPC timer.
fn outbound_transport_error(request: &JsonRpcMessage, message: &str) -> Option<JsonRpcMessage> {
    if let Some(error) = probe_transport_error(request, message) {
        return Some(error);
    }
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?.clone();
    if id.is_null() {
        return None;
    }
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32001, "message": message }
    }))
    .ok()
}

fn probe_compatibility_error(request: &JsonRpcMessage, message: &str) -> Option<JsonRpcMessage> {
    probe_error(request, message, false)
}

fn http_error_message(
    request: &JsonRpcMessage,
    status: u16,
    www_authenticate: Option<&str>,
    body: &str,
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?.clone();
    let marker = format!(
        "MCP_HTTP_STATUS={status};WWW_AUTHENTICATE={}",
        www_authenticate.unwrap_or_default()
    );
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32001,
            "message": marker,
            "data": {
                "httpStatus": status,
                "wwwAuthenticate": www_authenticate,
                "body": body,
                "nativeFailure":"transport",
                "diagnostic":format!("Error POSTing to endpoint: {body}")
            }
        }
    }))
    .ok()
}

#[cfg(test)]
mod remote_322_oracle_tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/mcp_remote_2_1_322.json")).unwrap()
    }

    #[test]
    fn response_mime_essence_matches_shipped_parser_including_quoted_controls() {
        for case in fixture()["mimeCases"].as_array().unwrap() {
            assert_eq!(
                case["value"]
                    .as_str()
                    .and_then(content_type_essence)
                    .as_deref(),
                case["essence"].as_str(),
                "{case:?}",
            );
        }
    }

    #[test]
    fn configured_header_merging_and_delete_match_shipped_transport() {
        let fixture = fixture();
        for case in fixture["headerCases"].as_array().unwrap() {
            let configured: lingxi_core::host::McpHeaders =
                serde_json::from_value(case["headers"].clone()).unwrap();
            let mut headers = build_headers(
                None,
                &configured,
                case["sessionId"].as_str(),
                case["protocolVersion"].as_str(),
            )
            .unwrap();
            if case["method"] == "initialize" {
                headers.remove("mcp-session-id");
            }
            // Product/version user-agent adaptation is outside these source
            // helper cases; Native supplies it through the owning host wrapper.
            headers.remove("user-agent");
            headers.remove("accept-encoding");
            let actual: serde_json::Map<String, serde_json::Value> = headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_str().unwrap().into()))
                .collect();
            assert_eq!(
                serde_json::Value::Object(actual),
                case["expected"],
                "{case:?}"
            );
        }
        let mut headers = build_common_headers(
            None,
            &lingxi_core::host::McpHeaders::new(),
            Some("wire-session-322"),
            Some("2025-11-25"),
        )
        .unwrap();
        headers.remove("user-agent");
        headers.remove("accept-encoding");
        let actual: serde_json::Map<String, serde_json::Value> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().into()))
            .collect();
        assert_eq!(serde_json::Value::Object(actual), fixture["deleteHeaders"]);
        for case in fixture["bodyHeaderCases"].as_array().unwrap() {
            let mut headers = HeaderMap::new();
            apply_body_derived_headers(&mut headers, &case["request"]);
            let actual: serde_json::Map<String, serde_json::Value> = headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_str().unwrap().into()))
                .collect();
            assert_eq!(
                serde_json::Value::Object(actual),
                case["expected"],
                "{case:?}"
            );
        }
    }

    #[test]
    fn json_response_arrays_preserve_native_source_values_and_validate_atomically() {
        for case in fixture()["jsonCases"].as_array().unwrap() {
            if !case["error"].is_null() {
                continue;
            }
            let decoded =
                decode_http_json_frames(case["responseRaw"].as_str().unwrap().as_bytes()).unwrap();
            let actual =
                Utf16JsonProjection::parse(&serde_json::to_string(&decoded).unwrap()).unwrap();
            let expected =
                Utf16JsonProjection::parse(case["receivedRaw"].as_str().unwrap()).unwrap();
            assert!(actual.semantically_eq(&expected), "{case:?}");
            assert_eq!(actual.string_units("/0/result/text").unwrap(), vec![0xd800]);
        }
        assert!(decode_http_json_frames(
            br#"[{"jsonrpc":"2.0","id":1,"result":{}},{"jsonrpc":"invalid","id":2,"result":{}}]"#
        )
        .is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_message_retains_local_stale_session_detail() {
        let request: JsonRpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {}
        }))
        .expect("request");
        let error = http_error_message(
            &request,
            400,
            None,
            "Server not initialized for this session",
        )
        .expect("error response");
        let value = serde_json::to_value(error).expect("serialize response");
        assert_eq!(value["error"]["data"]["httpStatus"], 400);
        assert_eq!(
            value["error"]["data"]["body"],
            "Server not initialized for this session"
        );
    }
}

#[cfg(test)]
mod modern_headers_oracle_tests {
    use super::*;

    #[test]
    fn derived_headers_match_pinned_upstream_execution() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../posix/tests/fixtures/mcp_negotiation_286_oracle.json"
        ))
        .unwrap();
        for case in fixture["headerCases"].as_array().unwrap() {
            let mut headers = HeaderMap::new();
            if let Some(initial) = case["initialHeaders"].as_object() {
                for (name, value) in initial {
                    headers.insert(
                        HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        HeaderValue::from_str(value.as_str().unwrap()).unwrap(),
                    );
                }
            }
            apply_body_derived_headers(&mut headers, &case["request"]);
            let actual: serde_json::Map<String, serde_json::Value> = headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        serde_json::Value::String(value.to_str().unwrap().to_string()),
                    )
                })
                .collect();
            assert_eq!(
                serde_json::Value::Object(actual),
                case["expected"],
                "{}",
                case["name"]
            );
        }
    }
}

#[cfg(test)]
mod input_required_cancel_tests {
    use super::*;
    use jsonrpc::router::PerRequestCancellation;

    #[test]
    fn request_tokens_are_bounded_by_queued_frames_and_released_when_taken() {
        let cancellation = HttpRequestCancellation::default();
        let id = jsonrpc::Id::Number(1);
        let token = cancellation.prepare(&id);
        assert_eq!(cancellation.pending.lock().unwrap().len(), 1);
        token.cancel();
        let frame = JsonRpcMessage::Request(jsonrpc::Request::new("tools/call", None, id));
        assert!(cancellation.take(&frame).is_cancelled());
        assert!(cancellation.pending.lock().unwrap().is_empty());
        // A replayed or closed queued request cannot mint a fresh live token.
        assert!(cancellation.take(&frame).is_cancelled());
    }

    #[test]
    fn failed_enqueue_and_connection_close_release_all_queued_tokens() {
        let cancellation = HttpRequestCancellation::default();
        let first = jsonrpc::Id::Number(1);
        let second = jsonrpc::Id::Number(2);
        let a = cancellation.prepare(&first);
        let b = cancellation.prepare(&second);
        cancellation.discard(&first);
        assert!(a.is_cancelled());
        assert_eq!(cancellation.pending.lock().unwrap().len(), 1);
        cancellation.close();
        assert!(b.is_cancelled());
        assert!(cancellation.pending.lock().unwrap().is_empty());
        let frame = JsonRpcMessage::Request(jsonrpc::Request::new("tools/call", None, second));
        assert!(cancellation.take(&frame).is_cancelled());
    }
}
