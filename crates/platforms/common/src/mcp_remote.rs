//! Shared remote MCP transport for mobile and desktop platforms.
//!
//! This module owns the HTTP/SSE JSON-RPC connection and the MCP protocol
//! state machine.  Process creation and child reaping intentionally stay in
//! the platform-specific stdio transport; callers only need to retain this
//! value and use [`RemoteMcpTransport::connection_for`] to bridge a live
//! connection into `mcp::McpClient`.

use async_trait::async_trait;
use jsonrpc::{Connection, ConnectionError, InboundHandler, Request, Response, RouterError};
use lingxi_core::host::mcp_result::{
    connection_error_diagnostic, drive_modern_request, result_error_diagnostic, JsonrpcMcpResultIo,
    McpInputRequiredOptions, McpResultError,
};
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError, McpIconDto,
    McpNegotiatedProtocol, McpNotificationDto, McpNotificationStream, McpPromptDto, McpProtocolEra,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpResourceTemplateDto,
    McpServerMetadataDto, McpToolAnnotationsDto, McpToolDto, McpToolResultDto, McpTransport,
    McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Legacy MCP protocol revision used when modern negotiation is unavailable.
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
/// Modern protocol revision used by the discovery probe.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
const CLIENT_DESCRIPTION: &str = "An agentic coding tool";
const MCP_CLIENT_NAME: &str = "lingxi";
const MCP_CLIENT_TITLE: &str = branding::PRODUCT_NAME;
const MCP_WEBSITE_URL: &str = "https://claude.com/claude-code";
const MCP_SKILLS_EXTENSION_KEY: &str = "io.modelcontextprotocol/skills";
const MAX_PROBE_TIMEOUT_MS: u64 = 5_000;

fn source_string(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust strings are valid UTF-16")
}

fn source_string_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text is derived from exact UTF-16 units")
}

fn source_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("generated object fields are unique")
}

fn empty_source_object() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("{}").expect("empty JSON object is valid")
}

fn null_source_value() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null JSON value is valid")
}

fn deserialize_required_source_string<'de, D>(
    deserializer: D,
) -> Result<Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let projection = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if projection.string_units("").is_some() {
        Ok(projection)
    } else {
        Err(serde::de::Error::custom("expected a JSON string"))
    }
}

fn deserialize_optional_source_string<'de, D>(
    deserializer: D,
) -> Result<Option<Utf16JsonProjection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let projection = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if projection
        .at_pointer("")
        .is_some_and(JsonValueView::is_null)
    {
        Ok(None)
    } else if projection.string_units("").is_some() {
        Ok(Some(projection))
    } else {
        Err(serde::de::Error::custom("expected a string or null"))
    }
}

fn source_json_payload(value: &Utf16JsonProjection) -> Result<jsonrpc::JsonPayload, McpError> {
    let json = value
        .to_json_string()
        .map_err(|error| McpError::Internal(error.to_string().into()))?;
    jsonrpc::JsonPayload::parse(json).map_err(internal_decode)
}

fn projection_from_payload(payload: jsonrpc::JsonPayload) -> Result<Utf16JsonProjection, McpError> {
    Utf16JsonProjection::parse(payload.as_json())
        .map_err(|error| McpError::Internal(error.to_string().into()))
}

fn projection_from_payload_ref(payload: &jsonrpc::JsonPayload) -> Option<Utf16JsonProjection> {
    Utf16JsonProjection::parse(payload.as_json()).ok()
}

fn payload_from_projection(value: &Utf16JsonProjection) -> Option<jsonrpc::JsonPayload> {
    value
        .to_json_string()
        .ok()
        .and_then(|source| jsonrpc::JsonPayload::parse(source).ok())
}

fn view_is_object(value: JsonValueView<'_>) -> bool {
    value.object_entries().is_some()
}

fn bounded_probe_timeout_ms(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(MAX_PROBE_TIMEOUT_MS)
        .min(MAX_PROBE_TIMEOUT_MS)
}

/// Shared remote HTTP/SSE MCP transport.
#[derive(Default)]
pub struct RemoteMcpTransport {
    proxy_auth_helper: Option<Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>>,
    connections: Arc<Mutex<HashMap<lingxi_core::types::McpConnectionId, Arc<Connection>>>>,
    negotiated: Arc<Mutex<HashMap<lingxi_core::types::McpConnectionId, McpNegotiatedProtocol>>>,
    metadata: Arc<Mutex<HashMap<lingxi_core::types::McpConnectionId, McpServerMetadataDto>>>,
    elicitation: Arc<
        Mutex<
            HashMap<
                lingxi_core::types::McpConnectionId,
                lingxi_core::host::McpElicitationCapabilities,
            >,
        >,
    >,
}

/// Synchronous cleanup for a cancelled connect/initialize future. The registry
/// wraps transport operations in a timeout; dropping that future must not leave
/// an HTTP broker (or an SSE GET) retained in the connection map.
struct RemoteConnectionCleanupGuard<'a> {
    transport: &'a RemoteMcpTransport,
    id: lingxi_core::types::McpConnectionId,
    armed: bool,
}

impl RemoteConnectionCleanupGuard<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for RemoteConnectionCleanupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.transport.remove(self.id);
        }
    }
}

impl RemoteMcpTransport {
    /// Construct an empty remote transport.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Share the host's process-local proxy-auth helper with HTTP and SSE
    /// MCP clients. The route builder applies its header only to Native's
    /// primary process proxy, never the registered-agent fallback.
    #[must_use]
    pub fn with_proxy_auth_helper(
        mut self,
        helper: Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>,
    ) -> Self {
        self.proxy_auth_helper = Some(helper);
        self
    }

    /// Return the live JSON-RPC connection owned by this transport.
    #[must_use]
    pub fn connection_for(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Option<Arc<Connection>> {
        self.connections.lock().ok()?.get(&id).cloned()
    }

    fn negotiated_for(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Option<McpNegotiatedProtocol> {
        self.negotiated.lock().ok()?.get(&id).cloned()
    }

    fn elicitation_for(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Result<lingxi_core::host::McpElicitationCapabilities, McpError> {
        self.elicitation
            .lock()
            .map_err(|_| McpError::Internal("MCP capability map poisoned".into()))?
            .get(&id)
            .copied()
            .ok_or_else(|| {
                McpError::Internal(format!("missing MCP capability authority for {id}").into())
            })
    }

    fn set_elicitation(
        &self,
        id: lingxi_core::types::McpConnectionId,
        capabilities: lingxi_core::host::McpElicitationCapabilities,
    ) -> Result<(), McpError> {
        self.elicitation
            .lock()
            .map_err(|_| McpError::Internal("MCP capability map poisoned".into()))?
            .insert(id, capabilities);
        Ok(())
    }

    fn remove(&self, id: lingxi_core::types::McpConnectionId) {
        if let Ok(mut connections) = self.connections.lock() {
            if let Some(connection) = connections.remove(&id) {
                connection.close();
            }
        }
        if let Ok(mut negotiated) = self.negotiated.lock() {
            negotiated.remove(&id);
        }
        if let Ok(mut metadata) = self.metadata.lock() {
            metadata.remove(&id);
        }
        if let Ok(mut elicitation) = self.elicitation.lock() {
            elicitation.remove(&id);
        }
    }

    fn adopt_metadata(
        &self,
        id: lingxi_core::types::McpConnectionId,
        metadata: McpServerMetadataDto,
    ) {
        if let Ok(mut map) = self.metadata.lock() {
            map.insert(id, metadata);
        }
    }

    fn remaining(deadline: tokio::time::Instant) -> Option<Duration> {
        deadline.checked_duration_since(tokio::time::Instant::now())
    }

    fn decorate_params(
        &self,
        id: lingxi_core::types::McpConnectionId,
        method: &str,
        params: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, McpError> {
        let Some(protocol) = self.negotiated_for(id) else {
            return Ok(params);
        };
        if protocol.era == McpProtocolEra::Modern && modern_request_requires_meta(method) {
            let mut params = if params.object_entries().is_some() {
                params
            } else {
                empty_source_object()
            };
            params
                .set_field(
                    "_meta",
                    modern_meta(&protocol.version, self.elicitation_for(id)?.modern),
                )
                .map_err(|error| McpError::Internal(error.to_string().into()))?;
            Ok(params)
        } else {
            Ok(params)
        }
    }

    async fn call_rpc(
        &self,
        conn: &McpRawConnection,
        method: &str,
        params: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id).into())
        })?;
        let params = self.decorate_params(conn.connection_id, method, params)?;
        if self.negotiated_for(conn.connection_id).is_some_and(|p| {
            p.era == McpProtocolEra::Modern && modern_request_requires_meta(method)
        }) {
            drive_modern_request(
                &JsonrpcMcpResultIo {
                    connection,
                    client_capabilities: modern_meta(
                        MODERN_PROTOCOL_VERSION,
                        self.elicitation_for(conn.connection_id)?.modern,
                    )
                    .subprojection("/io.modelcontextprotocol~1clientCapabilities")
                    .expect("generated client capabilities exist"),
                },
                method,
                params,
                McpInputRequiredOptions::default(),
            )
            .await
            .map_err(map_result_err)
        } else {
            let params = source_json_payload(&params)?;
            let result: jsonrpc::JsonPayload = connection
                .call(method, params)
                .await
                .map_err(|error| map_call_err(&error))?;
            projection_from_payload(result)
        }
    }

    async fn connect_before(
        &self,
        spec: &McpTransportSpec,
        deadline: tokio::time::Instant,
    ) -> Result<McpRawConnection, McpError> {
        let Some(remaining) = Self::remaining(deadline) else {
            return Err(McpError::Connection(
                "MCP connection deadline exceeded".into(),
            ));
        };
        tokio::time::timeout(remaining, self.connect(spec))
            .await
            .map_err(|_| McpError::Connection("MCP connection deadline exceeded".into()))?
    }

    async fn initialize_before(
        &self,
        conn: &McpRawConnection,
        version: &str,
        deadline: tokio::time::Instant,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        self.initialize_before_detailed(conn, version, deadline)
            .await
            .map_err(|(error, _)| error)
    }

    /// [`Self::initialize_before`], also handing back the HTTP detail behind a
    /// rejected handshake. Only the legacy-SSE fallback needs it; everything
    /// else goes through the plain form above.
    async fn initialize_before_detailed(
        &self,
        conn: &McpRawConnection,
        version: &str,
        deadline: tokio::time::Instant,
    ) -> Result<ServerCapabilitiesDto, (McpError, Option<HandshakeHttp>)> {
        let Some(remaining) = Self::remaining(deadline) else {
            return Err((
                McpError::Connection("MCP connection deadline exceeded".into()),
                None,
            ));
        };
        match tokio::time::timeout(
            remaining,
            self.initialize_with_version_detailed(conn, version, remaining),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err((
                McpError::Connection("MCP connection deadline exceeded".into()),
                None,
            )),
        }
    }

    async fn probe_modern(
        &self,
        conn: &McpRawConnection,
        deadline: tokio::time::Instant,
        probe_timeout: Duration,
    ) -> Result<Option<ModernDiscovery>, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id).into())
        })?;
        let mut corrective_retry = false;
        loop {
            let timeout = Self::remaining(deadline)
                .ok_or_else(|| McpError::Connection("MCP connection deadline exceeded".into()))?
                .min(probe_timeout);
            let params = source_json_payload(&modern_probe_params(
                self.elicitation_for(conn.connection_id)?.modern,
            ))?;
            match connection
                .call_with_timeout_probe::<_, jsonrpc::JsonPayload>(
                    "server/discover",
                    params,
                    timeout,
                )
                .await
            {
                Ok(reply) => return Ok(parse_modern_discovery(projection_from_payload(reply)?)),

                Err(ConnectionError::Router(RouterError::Remote(error))) => {
                    if matches!(http_status(&error.data), Some(401 | 403)) {
                        return Err(handshake_error(&ConnectionError::Router(
                            RouterError::Remote(error),
                        )));
                    }
                    match modern_probe_error(&error, corrective_retry)? {
                        ModernProbeErrorAction::Legacy => return Ok(None),
                        ModernProbeErrorAction::Retry => corrective_retry = true,
                    }
                }
                Err(ConnectionError::Router(
                    RouterError::Deserialize(_) | RouterError::WrongResponseId { .. },
                )) => return Ok(None),
                Err(error) => return Err(map_call_err(&error)),
            }
        }
    }

    async fn initialize_with_version(
        &self,
        conn: &McpRawConnection,
        version: &str,
        timeout: Duration,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        self.initialize_with_version_detailed(conn, version, timeout)
            .await
            .map_err(|(error, _)| error)
    }

    async fn initialize_with_version_detailed(
        &self,
        conn: &McpRawConnection,
        version: &str,
        timeout: Duration,
    ) -> Result<ServerCapabilitiesDto, (McpError, Option<HandshakeHttp>)> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            (
                McpError::Connection(format!("no such connection: {}", conn.connection_id).into()),
                None,
            )
        })?;
        let params = self
            .decorate_params(
                conn.connection_id,
                "initialize",
                initialize_params_for_version(
                    version,
                    self.elicitation_for(conn.connection_id)
                        .map_err(|error| (error, None))?
                        .legacy,
                ),
            )
            .map_err(|error| (error, None))?;
        let params = source_json_payload(&params).map_err(|error| (error, None))?;
        let result: jsonrpc::JsonPayload = connection
            .call_with_timeout("initialize", params, timeout)
            .await
            .map_err(|error| (handshake_error(&error), handshake_http(&error)))?;
        let result = projection_from_payload(result).map_err(|error| (error, None))?;

        let (dto, selected_version, metadata) =
            parse_legacy_initialize(result).map_err(|error| (error, None))?;
        if let Ok(mut map) = self.negotiated.lock() {
            map.insert(
                conn.connection_id,
                McpNegotiatedProtocol {
                    era: McpProtocolEra::Legacy,
                    version: selected_version,
                },
            );
        }
        self.adopt_metadata(conn.connection_id, metadata);
        connection
            .notify("notifications/initialized", json!({}))
            .map_err(|error| {
                (
                    McpError::Handshake(connection_error_diagnostic(&error)),
                    None,
                )
            })?;

        Ok(dto)
    }

    /// Install a freshly dialled connection: attach the ping handler and put it
    /// in the map. Shared by [`Self::connect`] and the legacy-SSE rescue so the
    /// rescue's connection is indistinguishable from any other.
    async fn register_connection(
        &self,
        connection: Connection,
    ) -> Result<McpRawConnection, McpError> {
        self.register_connection_with_id(lingxi_core::types::McpConnectionId::new(), connection)
            .await
    }

    async fn register_connection_with_id(
        &self,
        id: lingxi_core::types::McpConnectionId,
        connection: Connection,
    ) -> Result<McpRawConnection, McpError> {
        let connection = Arc::new(connection);
        connection
            .register_handler("ping", Arc::new(PingHandler))
            .await;
        if let Err(error) =
            self.set_elicitation(id, lingxi_core::host::McpElicitationCapabilities::default())
        {
            connection.close();
            return Err(error);
        }
        let installed = self
            .connections
            .lock()
            .map_err(|_| McpError::Internal("remote connection map poisoned".into()))
            .map(|mut connections| connections.insert(id, Arc::clone(&connection)));
        if let Err(error) = installed {
            connection.close();
            self.remove(id);
            return Err(error);
        }
        Ok(McpRawConnection { connection_id: id })
    }

    /// Upstream's legacy HTTP+SSE rescue: a streamable-HTTP server that rejects
    /// the `initialize` POST may simply not speak streamable HTTP, so re-dial it
    /// the old way before giving up.
    ///
    /// `None` means "not a rescue case, keep the original error". `Some` is the
    /// rescue's own verdict.
    ///
    /// Upstream guards the attempt four ways before it starts — a legacy
    /// factory exists for this config, the transport has not already negotiated
    /// a protocol version, [`rejection_invites_legacy_sse`] holds, and the flag
    /// is on (default TRUE, so this is live behaviour and not a dormant gate).
    async fn legacy_sse_rescue(
        &self,
        spec: &McpTransportSpec,
        http: Option<&HandshakeHttp>,
        original: &McpError,
        deadline: tokio::time::Instant,
        elicitation: lingxi_core::host::McpElicitationCapabilities,
    ) -> Option<Result<McpConnectResult, McpError>> {
        let McpTransportSpec::Http {
            url,
            headers,
            headers_helper,
            oauth,
        } = spec
        else {
            return None;
        };
        let http = http?;
        if !rejection_invites_legacy_sse(http) {
            return None;
        }
        if !telemetry::flag_bool(LEGACY_SSE_FALLBACK_FLAG, true) {
            return None;
        }
        let post_method_not_allowed = http.status == 405;
        tracing::warn!(
            status = http.status,
            "mcp: initialize POST rejected; trying legacy HTTP+SSE"
        );

        // `Math.min(5000, Math.max(1000, connectTimeout - elapsed))` — what is
        // left of the dial budget, floored so a nearly-spent deadline still
        // gets a real attempt, and capped so the rescue cannot become the whole
        // connect.
        let budget = Self::remaining(deadline)
            .unwrap_or_default()
            .clamp(LEGACY_SSE_MIN_BUDGET, LEGACY_SSE_MAX_BUDGET);
        let rescue_deadline = tokio::time::Instant::now() + budget;

        // Same url, same headers, same oauth: only the transport changes.
        // ⚠️ This is why the rescue lives here and not in the registry's
        // retry seam — `oauth::server_key` folds the spec KIND into the key,
        // so re-dialling through that seam with an `Sse` spec would silently
        // repartition the stored token and the discovery cache. Upstream keeps
        // its config and swaps only the transport object.
        let sse_spec = McpTransportSpec::Sse {
            url: url.clone(),
            headers: headers.clone(),
            headers_helper: headers_helper.clone(),
            oauth: oauth.clone(),
        };
        let negotiated = McpNegotiatedProtocol {
            era: McpProtocolEra::Legacy,
            version: MCP_PROTOCOL_VERSION.to_string(),
        };

        let rescue = async {
            let connection = self
                .connect_sse_rescue(&sse_spec, post_method_not_allowed, rescue_deadline)
                .await?;
            let guard = RemoteConnectionCleanupGuard {
                transport: self,
                id: connection.connection_id,
                armed: true,
            };
            self.set_elicitation(connection.connection_id, elicitation)?;
            if let Ok(mut map) = self.negotiated.lock() {
                map.insert(connection.connection_id, negotiated.clone());
            }
            let capabilities = self
                .initialize_before(&connection, &negotiated.version, rescue_deadline)
                .await?;
            guard.disarm();
            let negotiated = self
                .negotiated_for(connection.connection_id)
                .unwrap_or_else(|| negotiated.clone());
            Ok::<_, McpError>(McpConnectResult {
                connection,
                capabilities,
                negotiated,
            })
        }
        .await;

        match rescue {
            Ok(result) => {
                tracing::info!("mcp: connected over legacy HTTP+SSE");
                Some(Ok(result))
            }
            Err(rescue_error) => Some(Err(choose_rescue_error(
                original,
                &rescue_error,
                post_method_not_allowed,
            ))),
        }
    }

    /// The rescue's dial. Split out so the rescue reads as one sequence.
    async fn connect_sse_rescue(
        &self,
        sse_spec: &McpTransportSpec,
        post_method_not_allowed: bool,
        deadline: tokio::time::Instant,
    ) -> Result<McpRawConnection, McpError> {
        let McpTransportSpec::Sse { url, headers, .. } = sse_spec else {
            return Err(McpError::Internal(
                "legacy rescue built a non-SSE spec".into(),
            ));
        };
        let Some(remaining) = Self::remaining(deadline) else {
            return Err(McpError::Connection(
                "MCP connection deadline exceeded".into(),
            ));
        };
        let connection = tokio::time::timeout(
            remaining,
            crate::mcp_sse::connect_sse_with_proxy_auth_helper(
                url,
                None,
                headers,
                crate::mcp_sse::SseEndpointMode::LegacyRescue {
                    post_method_not_allowed,
                },
                self.proxy_auth_helper.clone(),
            ),
        )
        .await
        .map_err(|_| McpError::Connection("MCP connection deadline exceeded".into()))?
        .map_err(McpError::from)?;
        self.register_connection(connection).await
    }
}

#[async_trait]
impl McpTransport for RemoteMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let id = lingxi_core::types::McpConnectionId::new();
        let connection = match spec {
            // A configured `type: "sse"` server speaks the legacy HTTP+SSE
            // contract: it names its POST url in an `endpoint` event.
            McpTransportSpec::Sse { url, headers, .. } => {
                crate::mcp_sse::connect_sse_with_proxy_auth_helper(
                    url,
                    None,
                    headers,
                    crate::mcp_sse::SseEndpointMode::EndpointEvent,
                    self.proxy_auth_helper.clone(),
                )
                .await
                .map_err(McpError::from)?
            }
            McpTransportSpec::SseIde {
                url, auth_token, ..
            } => {
                let headers = lingxi_core::host::McpHeaders::default();
                // The IDE serves both directions on one url and sends no
                // `endpoint` event.
                crate::mcp_sse::connect_sse_with_proxy_auth_helper(
                    url,
                    auth_token.as_deref(),
                    &headers,
                    crate::mcp_sse::SseEndpointMode::SameUrl,
                    self.proxy_auth_helper.clone(),
                )
                .await
                .map_err(McpError::from)?
            }
            McpTransportSpec::WsIde {
                url, auth_token, ..
            } => {
                let url = url
                    .parse::<url::Url>()
                    .map_err(|error| McpError::Connection(error.to_string().into()))?;
                crate::mcp_ws::connect_ws_optional(url, auth_token.as_deref())
                    .await
                    .map_err(|error| McpError::Connection(error.to_string().into()))?
            }
            McpTransportSpec::Http { url, headers, .. } => {
                crate::mcp_http::connect_http_with_proxy_auth_helper(
                    url,
                    None,
                    headers,
                    Some(Duration::from_secs(60)),
                    self.proxy_auth_helper.clone(),
                )
                .await
                .map_err(McpError::from)?
            }
            other => return Err(McpError::UnsupportedTransport(other.transport_kind())),
        };
        let connection = self.register_connection_with_id(id, connection).await?;
        if let Err(error) = self.set_elicitation(
            id,
            lingxi_core::host::McpElicitationCapabilities::for_transport(spec.transport_kind()),
        ) {
            self.remove(id);
            return Err(error);
        }
        Ok(connection)
    }

    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(options.deadline_ms))
            .ok_or_else(|| McpError::Connection("MCP connection deadline overflow".into()))?;
        let requested = options.expected_era.unwrap_or(McpProtocolEra::Legacy);
        // HTTP probes in place. Preserve the session, broker and request IDs
        // through discovery, compatibility fallback and the live connection.
        let connection = self.connect_before(spec, deadline).await?;
        let live_guard = RemoteConnectionCleanupGuard {
            transport: self,
            id: connection.connection_id,
            armed: true,
        };
        self.set_elicitation(connection.connection_id, options.elicitation)?;
        if requested == McpProtocolEra::Modern {
            let probe_cap =
                Duration::from_millis(bounded_probe_timeout_ms(options.probe_timeout_ms));
            if let Some(discovery) = self.probe_modern(&connection, deadline, probe_cap).await? {
                let negotiated = McpNegotiatedProtocol {
                    era: McpProtocolEra::Modern,
                    version: discovery.version,
                };
                if let Ok(mut map) = self.negotiated.lock() {
                    map.insert(connection.connection_id, negotiated.clone());
                }
                self.adopt_metadata(connection.connection_id, discovery.metadata);
                live_guard.disarm();
                return Ok(McpConnectResult {
                    connection,
                    capabilities: discovery.capabilities,
                    negotiated,
                });
            }
        }
        match self
            .initialize_before_detailed(&connection, MCP_PROTOCOL_VERSION, deadline)
            .await
        {
            Ok(capabilities) => {
                let negotiated = self
                    .negotiated_for(connection.connection_id)
                    .ok_or_else(|| McpError::Handshake("missing negotiated protocol".into()))?;
                live_guard.disarm();
                Ok(McpConnectResult {
                    connection,
                    capabilities,
                    negotiated,
                })
            }
            Err((error, http)) => {
                // Close the rejected HTTP connection even when SSE rescue
                // succeeds; disarming this guard would leak its broker.
                drop(live_guard);
                if let Some(result) = self
                    .legacy_sse_rescue(spec, http.as_ref(), &error, deadline, options.elicitation)
                    .await
                {
                    return result;
                }
                Err(error)
            }
        }
    }

    fn server_metadata(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Option<McpServerMetadataDto> {
        self.metadata.lock().ok()?.get(&id).cloned()
    }

    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        if let Some(discovery) = self
            .server_metadata(conn.connection_id)
            .and_then(|metadata| metadata.discovery)
            .and_then(parse_modern_discovery)
        {
            return Ok(discovery.capabilities);
        }
        let version = self
            .negotiated_for(conn.connection_id)
            .map(|protocol| protocol.version)
            .unwrap_or_else(|| MCP_PROTOCOL_VERSION.to_string());
        self.initialize_with_version(conn, &version, Duration::from_secs(60))
            .await
    }

    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        let raw = self
            .call_rpc(conn, "tools/list", empty_source_object())
            .await?;
        let raw = raw
            .to_json_string()
            .map_err(|error| McpError::Internal(error.to_string().into()))?;
        let parsed: ToolsListResult = serde_json::from_str(&raw).map_err(internal_decode)?;
        Ok(parsed.tools.into_iter().map(RawTool::into_dto).collect())
    }

    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        let raw = self
            .call_rpc(conn, "resources/list", empty_source_object())
            .await?;
        let raw = raw
            .to_json_string()
            .map_err(|error| McpError::Internal(error.to_string().into()))?;
        let parsed: ResourcesListResult = serde_json::from_str(&raw).map_err(internal_decode)?;
        Ok(parsed
            .resources
            .into_iter()
            .map(|resource| McpResourceDto {
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime_type: resource.mime_type,
                meta: resource.meta,
            })
            .collect())
    }

    async fn list_resource_templates(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        let raw = self
            .call_rpc(conn, "resources/templates/list", empty_source_object())
            .await?;
        let raw = raw
            .to_json_string()
            .map_err(|error| McpError::Internal(error.to_string().into()))?;
        let parsed: ResourceTemplatesListResult =
            serde_json::from_str(&raw).map_err(internal_decode)?;
        Ok(parsed
            .resource_templates
            .into_iter()
            .map(|template| McpResourceTemplateDto {
                uri_template: template.uri_template,
                name: template.name,
                description: template.description,
                mime_type: template.mime_type,
                annotations: template.annotations,
                meta: template.meta,
            })
            .collect())
    }

    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        let raw = self
            .call_rpc(conn, "prompts/list", empty_source_object())
            .await?;
        let raw = raw
            .to_json_string()
            .map_err(|error| McpError::Internal(error.to_string().into()))?;
        let parsed: PromptsListResult = serde_json::from_str(&raw).map_err(internal_decode)?;
        Ok(parsed
            .prompts
            .into_iter()
            .map(|prompt| McpPromptDto {
                name: prompt.name,
                description: prompt.description,
                arguments: prompt
                    .arguments
                    .into_iter()
                    .map(|argument| lingxi_core::host::McpPromptArgumentDto {
                        name: argument.name,
                        description: argument.description,
                        required: argument.required,
                    })
                    .collect(),
            })
            .collect())
    }

    async fn call_tool(
        &self,
        conn: &McpRawConnection,
        tool: &str,
        input: Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id).into())
        })?;
        let timeout = Duration::from_secs(60);
        let params = self.decorate_params(
            conn.connection_id,
            "tools/call",
            source_object(vec![
                ("name".into(), source_string(tool)),
                ("arguments".into(), input),
            ]),
        )?;
        let raw: Utf16JsonProjection = if self
            .negotiated_for(conn.connection_id)
            .is_some_and(|p| p.era == McpProtocolEra::Modern)
        {
            drive_modern_request(
                &JsonrpcMcpResultIo {
                    connection,
                    client_capabilities: modern_meta(
                        MODERN_PROTOCOL_VERSION,
                        self.elicitation_for(conn.connection_id)?.modern,
                    )
                    .subprojection("/io.modelcontextprotocol~1clientCapabilities")
                    .expect("generated client capabilities exist"),
                },
                "tools/call",
                params,
                McpInputRequiredOptions {
                    per_request_timeout: timeout,
                    max_total_timeout: Some(timeout),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| match error {
                McpResultError::Sdk(error) if error.code == "REQUEST_TIMEOUT" => {
                    McpError::Timeout {
                        server: String::new(),
                        tool: tool.to_owned(),
                        secs: timeout.as_secs(),
                    }
                }
                error => map_result_err(error),
            })?
        } else {
            let params = source_json_payload(&params)?;
            let result: jsonrpc::JsonPayload = connection
                .call_with_timeout("tools/call", params, timeout)
                .await
                .map_err(|error| match &error {
                    ConnectionError::Router(RouterError::Timeout(_)) => McpError::Timeout {
                        server: String::new(),
                        tool: tool.to_owned(),
                        secs: timeout.as_secs(),
                    },
                    ConnectionError::Router(RouterError::Remote(remote))
                        if remote.code == jsonrpc::METHOD_NOT_FOUND =>
                    {
                        McpError::ToolNotFound(tool.to_owned())
                    }
                    _ => map_call_err(&error),
                })?;
            projection_from_payload(result)?
        };
        let raw = raw
            .to_json_string()
            .map_err(|error| McpError::Internal(error.to_string().into()))?;
        let parsed: ToolCallResult = serde_json::from_str(&raw).map_err(internal_decode)?;

        Ok(McpToolResultDto {
            content: parsed.content,
            is_error: parsed.is_error,
            error: parsed.error,
            meta: parsed.meta,
            structured_content: parsed.structured_content,
        })
    }

    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        let raw = self
            .call_rpc(
                conn,
                "resources/read",
                source_object(vec![("uri".into(), uri.clone())]),
            )
            .await?;
        let raw = raw
            .to_json_string()
            .map_err(|error| McpError::Internal(error.to_string().into()))?;
        let parsed: ResourceReadResult = serde_json::from_str(&raw).map_err(internal_decode)?;
        let first = parsed
            .contents
            .into_iter()
            .next()
            .ok_or_else(|| McpError::Internal("resources/read returned no contents".into()))?;
        Ok(McpResourceContentDto {
            uri: first.uri.unwrap_or_else(|| uri.clone()),
            content: first
                .text
                .or(first.blob)
                .unwrap_or_else(|| source_string("")),
            mime_type: first.mime_type,
            meta: first.meta,
        })
    }

    async fn ping(&self, conn_id: lingxi_core::types::McpConnectionId) -> Result<(), McpError> {
        let _: Utf16JsonProjection = self
            .call_rpc(
                &McpRawConnection {
                    connection_id: conn_id,
                },
                "ping",
                empty_source_object(),
            )
            .await?;
        Ok(())
    }

    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id).into())
        })?;
        let stream =
            futures::stream::unfold(connection.notifications(), |mut receiver| async move {
                loop {
                    match receiver.recv().await {
                        Ok(notification) => {
                            let params = match notification.params {
                                Some(params) => {
                                    match Utf16JsonProjection::parse(params.as_json()) {
                                        Ok(params) => params,
                                        Err(_) => continue,
                                    }
                                }
                                None => null_source_value(),
                            };
                            return Some((
                                McpNotificationDto {
                                    method: source_string_units(
                                        notification.method.code_units().to_vec(),
                                    ),
                                    params,
                                },
                                receiver,
                            ));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            });
        Ok(Box::pin(stream))
    }

    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Err(McpError::Internal(
            "remote mcp elicitation delegated to lingxi-mcp::McpClient".into(),
        ))
    }

    async fn disconnect(
        &self,
        conn_id: lingxi_core::types::McpConnectionId,
    ) -> Result<(), McpError> {
        self.remove(conn_id);
        Ok(())
    }

    fn disconnect_sync(&self, conn_id: lingxi_core::types::McpConnectionId) {
        self.remove(conn_id);
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![
            McpTransportKind::Sse,
            McpTransportKind::Http,
            McpTransportKind::SseIde,
            McpTransportKind::WsIde,
        ]
    }
}

struct PingHandler;

#[async_trait]
impl InboundHandler for PingHandler {
    async fn handle(&self, request: Request) -> Response {
        Response::success(request.id, json!({}))
    }
}

/// Validated discovery evidence adopted when a modern probe succeeds.
#[derive(Debug, Clone)]
pub struct ModernDiscovery {
    /// First supported revision in client preference order.
    pub version: String,
    /// Server capabilities from discovery, without an initialize exchange.
    pub capabilities: ServerCapabilitiesDto,
    /// Server identity, instructions and the normalized discovery response.
    pub metadata: McpServerMetadataDto,
}

const LEGACY_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";

fn source_fields(value: &Utf16JsonProjection, fields: &[&str]) -> Utf16JsonProjection {
    value
        .pick_object_fields(fields)
        .expect("source field projection expects an object")
}

fn source_member(value: &Utf16JsonProjection, key: &str) -> Option<Utf16JsonProjection> {
    value
        .subprojection_property_units(&key.encode_utf16().collect::<Vec<_>>())
        .ok()
}

fn source_array(value: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::array(value).expect("projected source array is valid")
}

fn source_number(value: u64) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&value.to_string()).expect("generated integer is valid JSON")
}

fn source_property_is_string(value: JsonValueView<'_>, key: &str) -> bool {
    value
        .get_property(key)
        .is_some_and(|value| value.as_str().is_some())
}

fn normalize_icons(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let length = value.array_len("")?;
    let mut icons = Vec::with_capacity(length);
    for index in 0..length {
        let icon = value.subprojection(&format!("/{index}")).ok()?;
        let root = icon.at_pointer("")?;
        if !source_property_is_string(root, "src")
            || root
                .get_property("mimeType")
                .is_some_and(|value| value.as_str().is_none())
            || root.get_property("theme").is_some_and(|theme| {
                !theme
                    .as_str()
                    .is_some_and(|theme| matches!(theme, "light" | "dark"))
            })
        {
            return None;
        }
        if let Some(sizes) = root.get_property("sizes") {
            if !sizes.array_len().is_some_and(|length| {
                (0..length).all(|index| {
                    sizes
                        .get_index(index)
                        .is_some_and(|size| size.as_str().is_some())
                })
            }) {
                return None;
            }
        }
        icons.push(source_fields(&icon, &["src", "mimeType", "sizes", "theme"]));
    }
    Some(source_array(icons))
}

fn normalize_server_info(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let source = value.at_pointer("")?;
    if !source_property_is_string(source, "name") || !source_property_is_string(source, "version") {
        return None;
    }
    for field in ["title", "description", "websiteUrl"] {
        if source
            .get_property(field)
            .is_some_and(|value| value.as_str().is_none())
        {
            return None;
        }
    }
    let mut info = source_fields(
        value,
        &["name", "version", "title", "description", "websiteUrl"],
    );
    if source.get_property("icons").is_some() {
        let icons = normalize_icons(&value.subprojection("/icons").ok()?)?;
        info.set_field("icons", icons).ok()?;
    }
    Some(info)
}

fn normalize_capabilities(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let root = value.at_pointer("")?;
    if !view_is_object(root) {
        return None;
    }
    let mut result = empty_source_object();

    for key in [
        "experimental",
        "logging",
        "completions",
        "prompts",
        "resources",
        "tools",
        "extensions",
        "tasks",
    ] {
        let Some(member) = root.get_property(key) else {
            continue;
        };
        if key == "tasks" {
            continue;
        }
        if !view_is_object(member) {
            return None;
        }
        let normalized = match key {
            "tools" | "resources" | "prompts" => {
                let object = value.subprojection(&format!("/{key}")).ok()?;
                let fields = source_fields(
                    &object,
                    if key == "resources" {
                        &["listChanged", "subscribe"]
                    } else {
                        &["listChanged"]
                    },
                );
                for field in if key == "resources" {
                    &["listChanged", "subscribe"][..]
                } else {
                    &["listChanged"][..]
                } {
                    if root
                        .get_property(key)
                        .and_then(|member| member.get_property(field))
                        .is_some_and(|value| value.as_bool().is_none())
                    {
                        return None;
                    }
                }
                fields
            }
            "extensions" | "experimental" => {
                let object = value.subprojection(&format!("/{key}")).ok()?;
                let entries = object.object_entries()?;
                if entries.iter().any(|entry| !view_is_object(entry.value)) {
                    return None;
                }
                object
            }
            _ => value.subprojection(&format!("/{key}")).ok()?,
        };
        result.set_field(key, normalized).ok()?;
    }
    Some(result)
}

fn capabilities_from_source(
    capabilities: &Utf16JsonProjection,
    raw_capabilities: Option<&Utf16JsonProjection>,
) -> ServerCapabilitiesDto {
    let root = capabilities
        .at_pointer("")
        .expect("capabilities are an object");
    let experimental = capabilities
        .subprojection("/experimental")
        .unwrap_or_else(|_| empty_source_object());
    let extensions = capabilities
        .subprojection("/extensions")
        .unwrap_or_else(|_| empty_source_object());
    ServerCapabilitiesDto {
        tools: root.get_property("tools").is_some(),
        resources: root.get_property("resources").is_some(),
        prompts: root.get_property("prompts").is_some(),
        logging: root.get_property("logging").is_some(),
        directory_read: directory_read_capability(raw_capabilities),
        experimental,
        extensions,
    }
}

/// Decode the modern discovery dispatch schema (2.1.286 `Wc`/`Ao`).
/// `resultType` remains opaque here; discovery has its own envelope rules.
pub fn parse_modern_discovery(mut reply: Utf16JsonProjection) -> Option<ModernDiscovery> {
    let (has_meta, ttl_ms, valid_cache_scope) = {
        let root = reply.at_pointer("")?;
        if !view_is_object(root) {
            return None;
        }
        let versions = root.get_property("supportedVersions")?;
        let count = versions.array_len()?;
        let mut has_modern_version = false;
        for index in 0..count {
            let version = versions.get_index(index)?.as_str()?;
            has_modern_version |= version == MODERN_PROTOCOL_VERSION;
        }
        let instructions = root.get_property("instructions");
        if !has_modern_version || instructions.is_some_and(|value| value.as_str().is_none()) {
            return None;
        }
        let ttl_ms = root
            .get_property("ttlMs")
            .and_then(|ttl| ttl.as_number())
            .filter(|ttl| {
                ttl.is_finite()
                    && *ttl >= 0.0
                    && ttl.fract() == 0.0
                    && *ttl <= 9_007_199_254_740_991.0
            })
            .map(|ttl| ttl as u64)
            .unwrap_or(0);
        let valid_cache_scope = root.get_property("cacheScope").is_some_and(|scope| {
            scope
                .as_str()
                .is_some_and(|scope| matches!(scope, "public" | "private"))
        });
        (
            root.get_property("_meta").is_some(),
            ttl_ms,
            valid_cache_scope,
        )
    };
    let capabilities = normalize_capabilities(&reply.subprojection("/capabilities").ok()?)?;
    reply.set_field("capabilities", capabilities.clone()).ok()?;

    let server_info = if has_meta {
        let mut meta = reply.subprojection("/_meta").ok()?;
        if !view_is_object(meta.at_pointer("")?) {
            return None;
        }
        let info =
            source_member(&meta, SERVER_INFO_KEY).and_then(|info| normalize_server_info(&info));
        meta.remove_field(SERVER_INFO_KEY).ok()?;
        if let Some(info) = &info {
            meta.set_field(SERVER_INFO_KEY, info.clone()).ok()?;
        }
        reply.set_field("_meta", meta).ok()?;
        info
    } else {
        None
    };

    reply.set_field("ttlMs", source_number(ttl_ms)).ok()?;
    if !valid_cache_scope {
        reply
            .set_field("cacheScope", source_string("private"))
            .ok()?;
    }
    let instructions = reply.subprojection("/instructions").ok();
    Some(ModernDiscovery {
        version: MODERN_PROTOCOL_VERSION.to_string(),
        capabilities: capabilities_from_source(&capabilities, Some(&capabilities)),
        metadata: McpServerMetadataDto {
            server_info,
            instructions,
            raw_capabilities: Some(capabilities),
            discovery: Some(reply),
        },
    })
}

/// Validate the server-selected legacy revision before acknowledging initialization.
pub fn parse_legacy_initialize(
    reply: Utf16JsonProjection,
) -> Result<(ServerCapabilitiesDto, String, McpServerMetadataDto), McpError> {
    let version = reply.string_at("/protocolVersion").ok_or_else(|| {
        McpError::Handshake("initialize result missing `protocolVersion` string".into())
    })?;
    if !LEGACY_PROTOCOL_VERSIONS.contains(&version) {
        return Err(McpError::Handshake(
            format!("Server's protocol version is not supported: {version}").into(),
        ));
    }
    let caps = reply
        .subprojection("/capabilities")
        .ok()
        .filter(|value| value.object_entries().is_some())
        .ok_or_else(|| {
            McpError::Handshake("initialize result missing or invalid `capabilities` object".into())
        })?;
    let server_info = reply
        .subprojection("/serverInfo")
        .ok()
        .and_then(|info| normalize_server_info(&info))
        .ok_or_else(|| {
            McpError::Handshake("initialize result missing or invalid `serverInfo`".into())
        })?;
    if reply
        .get_property("instructions")
        .is_some_and(|instructions| instructions.as_str().is_none())
    {
        return Err(McpError::Handshake(
            "initialize result invalid `instructions`".into(),
        ));
    }
    Ok((
        capabilities_from_source(&caps, Some(&caps)),
        version.to_string(),
        McpServerMetadataDto {
            server_info: Some(server_info),
            raw_capabilities: Some(caps),
            instructions: reply.subprojection("/instructions").ok(),
            discovery: None,
        },
    ))
}

/// Next action for a probe JSON-RPC error.
pub enum ModernProbeErrorAction {
    /// The server supplied no incompatible modern revision evidence.
    Legacy,
    /// Retry the shared modern revision once.
    Retry,
}

/// Match the upstream supported-version corrective retry and fallback rules.
pub fn modern_probe_error(
    error: &jsonrpc::ResponseError,
    retried: bool,
) -> Result<ModernProbeErrorAction, McpError> {
    let data = error.data.as_ref().and_then(projection_from_payload_ref);
    if data
        .as_ref()
        .and_then(|data| data.at_pointer(""))
        .and_then(|root| root.get_property("probeTransportFailure"))
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        let mut units = "Version negotiation probe failed: "
            .encode_utf16()
            .collect::<Vec<_>>();
        units.extend_from_slice(error.message.code_units());
        return Err(McpError::Connection(source_string_units(units)));
    }
    // HTTP rejections carry a bounded response body through the HTTP adapter.
    // If that body is itself a JSON-RPC error, use its version evidence.
    if data
        .as_ref()
        .and_then(|data| data.get_property("httpStatus"))
        .is_some()
    {
        let body_error = data
            .as_ref()
            .and_then(|data| data.get_property("body"))
            .and_then(|body| body.code_units())
            .and_then(|body| Utf16JsonProjection::parse_utf16_json(body).ok())
            .and_then(|body| body.subprojection("/error").ok())
            .and_then(|source| {
                let code = source.number_at("/code")?;
                const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
                if !code.is_finite()
                    || code.fract() != 0.0
                    || !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&code)
                {
                    return None;
                }
                let message = jsonrpc::messages::JsonRpcString::from_code_units(
                    source.string_units("/message")?,
                );
                let data = source
                    .subprojection("/data")
                    .ok()
                    .and_then(|value| payload_from_projection(&value));
                Some(jsonrpc::ResponseError {
                    code: code as i64,
                    message,
                    data,
                })
            });
        if let Some(body_error) = body_error {
            return modern_probe_error(&body_error, retried);
        }
        return Ok(ModernProbeErrorAction::Legacy);
    }
    if error.code != -32022 {
        return Ok(ModernProbeErrorAction::Legacy);
    }
    let Some(supported) = data
        .as_ref()
        .and_then(|data| data.get_property("supported"))
        .and_then(|supported| {
            let length = supported.array_len()?;
            if length == 0 {
                return None;
            }
            (0..length)
                .map(|index| supported.get_index(index)?.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
    else {
        return Ok(ModernProbeErrorAction::Legacy);
    };
    if supported
        .iter()
        .any(|version| version == MODERN_PROTOCOL_VERSION)
        && !retried
    {
        return Ok(ModernProbeErrorAction::Retry);
    }
    if supported
        .iter()
        .any(|version| version.as_str() >= MODERN_PROTOCOL_VERSION)
    {
        let mut units = "MCP unsupported protocol version: "
            .encode_utf16()
            .collect::<Vec<_>>();
        units.extend_from_slice(error.message.code_units());
        units.extend(format!("; supported={supported:?}").encode_utf16());
        return Err(McpError::Handshake(source_string_units(units)));
    }
    Ok(ModernProbeErrorAction::Legacy)
}

/// Build the protocol-versioned initialize request shared by remote and stdio.
pub fn initialize_params_for_version(
    version: &str,
    elicitation: lingxi_core::host::McpElicitationMode,
) -> Utf16JsonProjection {
    source_object(vec![
        ("protocolVersion".into(), source_string(version)),
        (
            "capabilities".into(),
            lingxi_core::host::mcp_client_capabilities(elicitation),
        ),
        (
            "clientInfo".into(),
            source_object(vec![
                ("name".into(), source_string(MCP_CLIENT_NAME)),
                ("title".into(), source_string(MCP_CLIENT_TITLE)),
                ("version".into(), source_string(env!("CARGO_PKG_VERSION"))),
                ("description".into(), source_string(CLIENT_DESCRIPTION)),
                ("websiteUrl".into(), source_string(MCP_WEBSITE_URL)),
            ]),
        ),
    ])
}

/// Build the modern `server/discover` request envelope.
pub fn modern_probe_params(
    elicitation: lingxi_core::host::McpElicitationMode,
) -> Utf16JsonProjection {
    source_object(vec![(
        "_meta".into(),
        modern_meta(MODERN_PROTOCOL_VERSION, elicitation),
    )])
}

/// Build the closed modern request metadata envelope.
pub fn modern_meta(
    version: &str,
    elicitation: lingxi_core::host::McpElicitationMode,
) -> Utf16JsonProjection {
    source_object(vec![
        (
            "io.modelcontextprotocol/protocolVersion".into(),
            source_string(version),
        ),
        (
            "io.modelcontextprotocol/clientInfo".into(),
            source_object(vec![
                ("name".into(), source_string(MCP_CLIENT_NAME)),
                ("title".into(), source_string(MCP_CLIENT_TITLE)),
                ("version".into(), source_string(env!("CARGO_PKG_VERSION"))),
                ("description".into(), source_string(CLIENT_DESCRIPTION)),
                ("websiteUrl".into(), source_string(MCP_WEBSITE_URL)),
            ]),
        ),
        (
            "io.modelcontextprotocol/clientCapabilities".into(),
            lingxi_core::host::mcp_client_capabilities(elicitation),
        ),
    ])
}

/// Whether a request carries the modern metadata envelope.
pub fn modern_request_requires_meta(method: &str) -> bool {
    matches!(
        method,
        "tools/list"
            | "tools/call"
            | "subscriptions/listen"
            | "completion/complete"
            | "resources/list"
            | "resources/templates/list"
            | "resources/read"
            | "resources/directory/read"
            | "prompts/list"
            | "prompts/get"
    )
}

/// Extract the MCP skills directory-read extension flag.
pub fn directory_read_capability(capabilities: Option<&Utf16JsonProjection>) -> bool {
    capabilities
        .and_then(|value| value.get_property("extensions"))
        .and_then(|value| value.get_property(MCP_SKILLS_EXTENSION_KEY))
        .and_then(|value| value.get_property("directoryRead"))
        .and_then(|value| value.as_bool())
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::{bounded_probe_timeout_ms, RawResourceTemplate, RawTool};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    fn source(value: &serde_json::Value) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(&value.to_string()).expect("fixture is valid JSON")
    }

    #[test]
    fn caller_probe_timeout_is_clamped_to_shared_remote_cap() {
        assert_eq!(bounded_probe_timeout_ms(None), 5_000);
        assert_eq!(bounded_probe_timeout_ms(Some(1_000)), 1_000);
        assert_eq!(bounded_probe_timeout_ms(Some(50_000)), 5_000);
    }

    #[test]
    fn tool_retrieval_hints_survive_remote_transport_decode() {
        let raw_json = serde_json::json!({
            "name": "search",
            "description": "Search records",
            "inputSchema": { "type": "object" },
            "annotations": { "readOnlyHint": true, "vendor/riskTier": "reviewed" },
            "icons": [{ "src": "https://example.invalid/icon.svg", "vendor/accent": "blue" }],
            "_meta": {
                "anthropic/searchHint": "records lookup",
                "anthropic/alwaysLoad": true,
                "vendor/opaque": { "keep": [1, 2, 3] }
            }
        })
        .to_string();
        let raw: RawTool = serde_json::from_str(&raw_json).expect("decode raw tool");

        let dto = raw.into_dto();
        assert_eq!(
            dto.search_hint
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("records lookup")
        );
        assert_eq!(dto.always_load, Some(true));
        assert_eq!(
            dto.annotations
                .as_ref()
                .and_then(|annotations| annotations.extra.string_at("/vendor~1riskTier")),
            Some("reviewed")
        );
        assert_eq!(
            dto.icons[0].extra.string_at("/vendor~1accent"),
            Some("blue")
        );
        assert!(dto
            .meta
            .as_ref()
            .unwrap()
            .semantically_eq(&source(&serde_json::json!({
                "anthropic/searchHint": "records lookup",
                "anthropic/alwaysLoad": true,
                "vendor/opaque": { "keep": [1, 2, 3] }
            }))));
    }

    #[test]
    fn resource_template_metadata_survives_remote_transport_decode() {
        let raw_json = serde_json::json!({
            "uriTemplate": "file:///{path}",
            "name": "file",
            "description": "Workspace file",
            "mimeType": "text/plain",
            "annotations": { "audience": ["assistant"], "vendor/rank": 7 },
            "_meta": { "vendor/template": "opaque" }
        })
        .to_string();
        let raw: RawResourceTemplate =
            serde_json::from_str(&raw_json).expect("decode resource template");

        assert_eq!(
            raw.annotations.as_ref().unwrap().number_at("/vendor~1rank"),
            Some(7.0)
        );
        assert_eq!(
            raw.meta.as_ref().unwrap().string_at("/vendor~1template"),
            Some("opaque")
        );
    }

    #[test]
    fn resource_prompt_and_instruction_strings_keep_exact_utf16_units() {
        let resources: super::ResourcesListResult = serde_json::from_str(
            r#"{"resources":[{"uri":"file://\ud800","name":"resource\udfff","description":"detail\ud800"}]}"#,
        )
        .expect("resource text fields use source projections");
        let resource = resources.resources.into_iter().next().unwrap();
        assert_eq!(
            resource.uri.string_units(""),
            Some("file://".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            resource.name.string_units(""),
            Some("resource".encode_utf16().chain([0xdfff]).collect())
        );
        assert_eq!(
            resource.description.unwrap().string_units(""),
            Some("detail".encode_utf16().chain([0xd800]).collect())
        );

        let prompts: super::PromptsListResult = serde_json::from_str(
            r#"{"prompts":[{"name":"prompt\ud800","description":"about\udfff","arguments":[{"name":"arg\ud800","description":"argument\udfff"}]}]}"#,
        )
        .expect("prompt text fields use source projections");
        let prompt = prompts.prompts.into_iter().next().unwrap();
        assert_eq!(
            prompt.name.string_units(""),
            Some("prompt".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            prompt.description.unwrap().string_units(""),
            Some("about".encode_utf16().chain([0xdfff]).collect())
        );
        assert_eq!(
            prompt.arguments[0].name.string_units(""),
            Some("arg".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            prompt.arguments[0]
                .description
                .as_ref()
                .unwrap()
                .string_units(""),
            Some("argument".encode_utf16().chain([0xdfff]).collect())
        );

        let discovery = super::parse_modern_discovery(
            Utf16JsonProjection::parse(
                r#"{"supportedVersions":["2026-07-28"],"capabilities":{},"instructions":"use\ud800"}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            discovery.metadata.instructions.unwrap().string_units(""),
            Some("use".encode_utf16().chain([0xd800]).collect())
        );
    }

    #[test]
    fn tool_wire_source_members_keep_unpaired_surrogate_units() {
        let raw: RawTool = serde_json::from_str(
            r#"{"name":"source","description":"before\ud800after","inputSchema":{"title":"\udfff"},"_meta":{"vendor":"\ud800"}}"#,
        )
        .expect("JSON raw-value adapters preserve escaped UTF-16 source");
        let dto = raw.into_dto();
        assert_eq!(
            dto.description.string_units(""),
            Some(
                "before"
                    .encode_utf16()
                    .chain([0xd800])
                    .chain("after".encode_utf16())
                    .collect()
            )
        );
        assert_eq!(dto.input_schema.string_units("/title"), Some(vec![0xdfff]));
        assert_eq!(
            dto.meta.unwrap().string_units("/vendor"),
            Some(vec![0xd800])
        );
    }

    #[test]
    fn tool_result_source_members_keep_unpaired_surrogate_units() {
        let result: super::ToolCallResult = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"\ud800"}],"error":"\udfff","_meta":{"vendor":"\udfff"},"structuredContent":{"value":"\ud800"}}"#,
        )
        .expect("JSON raw-value adapters preserve escaped UTF-16 source");
        assert_eq!(result.content.string_units("/0/text"), Some(vec![0xd800]));
        assert_eq!(
            result
                .error
                .as_ref()
                .and_then(|error| error.string_units("")),
            Some(vec![0xdfff])
        );
        assert_eq!(
            result.meta.unwrap().string_units("/vendor"),
            Some(vec![0xdfff])
        );
        assert_eq!(
            result.structured_content.unwrap().string_units("/value"),
            Some(vec![0xd800])
        );
    }
}

/// Decode the wire capability presence map into the shared Source DTO.
pub fn capabilities_from_wire(
    capabilities: &Utf16JsonProjection,
    raw_capabilities: Option<&Utf16JsonProjection>,
) -> ServerCapabilitiesDto {
    capabilities_from_source(capabilities, raw_capabilities)
}

fn map_result_err(error: McpResultError) -> McpError {
    match error {
        McpResultError::Connection(error) => map_call_err(&error),
        McpResultError::Sdk(error) => McpError::Result(error),
        error @ McpResultError::Local(_) => McpError::Internal(result_error_diagnostic(&error)),
    }
}

fn map_call_err(error: &ConnectionError) -> McpError {
    McpError::Internal(connection_error_diagnostic(error))
}

fn internal_decode(error: serde_json::Error) -> McpError {
    McpError::Internal(error.to_string().into())
}

fn source_contains_ascii(source: &Utf16JsonProjection, needle: &str) -> bool {
    let Some(units) = source.string_units("") else {
        return false;
    };
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    let fold_ascii = |unit: u16| {
        if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
            unit + u16::from(b'a' - b'A')
        } else {
            unit
        }
    };
    units.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(&needle)
            .all(|(actual, expected)| fold_ascii(*actual) == fold_ascii(*expected))
    })
}

fn http_status(data: &Option<jsonrpc::JsonPayload>) -> Option<u16> {
    let data = data.as_ref().and_then(projection_from_payload_ref)?;
    let status = data.number_at("/httpStatus")?;
    if !status.is_finite() || status.fract() != 0.0 || status < 0.0 || status > u16::MAX as f64 {
        return None;
    }
    Some(status as u16)
}

/// `tengu_mcp_legacy_sse_fallback`. Default TRUE upstream, so this is live
/// behaviour rather than a dormant gate.
const LEGACY_SSE_FALLBACK_FLAG: &str = "tengu_mcp_legacy_sse_fallback";
/// `Math.max(1000, …)` — a nearly-spent deadline still gets a real attempt.
const LEGACY_SSE_MIN_BUDGET: Duration = Duration::from_millis(1000);
/// `Math.min(Go, …)` with `Go = 5000` — the rescue cannot become the connect.
const LEGACY_SSE_MAX_BUDGET: Duration = Duration::from_millis(5000);

/// Which error a failed rescue surfaces.
///
/// Upstream, where `qe` is "the rescue dial timed out", `dt` is `Fo(...)`
/// ("this failure is auth-ish"), `L` is `postMethodNotAllowed`, `E` the
/// original POST rejection and `Ie` the rescue's own error:
///
/// ```js
/// if (qe && L && Xe) throw new gb("… requires authorization …");
/// if (qe || (Ie instanceof M$t && !dt) || (dt && !L)) throw E;
/// throw Ie;
/// ```
///
/// The default is deliberately the ORIGINAL rejection: unless the rescue
/// produced evidence that this really is a legacy SSE server, the user should
/// see the problem they actually have, not a confusing SSE error on a url that
/// was never an SSE endpoint.
///
/// ⚠️ Approximated in one place. Upstream's `sawAuthChallenge` is set by a fetch
/// wrapper, so it sees a 401/403 on ANY request in the dial; here only the
/// dial's final error is observable, so "a NEW auth challenge appeared" reads
/// as "the rescue failed auth-ish". A rejection that reaches this function is
/// always a 400/404/405 (the predicate admits nothing else), so the original
/// dial's own final error can never have been the 401/403 that would make the
/// difference — the two agree except when a 401/403 occurred on some earlier
/// request of the original dial and was then followed by one of those statuses.
fn choose_rescue_error(
    original: &McpError,
    rescue: &McpError,
    post_method_not_allowed: bool,
) -> McpError {
    let timed_out = matches!(rescue, McpError::Connection(message)
        if source_contains_ascii(message, "deadline exceeded"));
    let auth_ish = matches!(
        rescue,
        McpError::HttpResponse {
            status: 401 | 403,
            ..
        }
    ) || matches!(rescue, McpError::OAuth(_));
    let structured_http = matches!(rescue, McpError::HttpResponse { .. });

    if timed_out && post_method_not_allowed && auth_ish {
        return McpError::Connection(
            "MCP server requires authorization: its legacy HTTP+SSE stream answered with an \
             auth challenge and the authorization flow did not finish within the connect budget"
                .into(),
        );
    }
    if timed_out || (structured_http && !auth_ish) || (auth_ish && !post_method_not_allowed) {
        return clone_original(original);
    }
    clone_original(rescue)
}

/// `McpError` is not `Clone`; the two the rescue picks between are both simple
/// enough to rebuild by shape.
fn clone_original(error: &McpError) -> McpError {
    match error {
        McpError::HttpResponse {
            status,
            www_authenticate,
        } => McpError::HttpResponse {
            status: *status,
            www_authenticate: www_authenticate.clone(),
        },
        // Preserve the variant rather than re-wrapping, or the message picks up
        // a second "connection failed:" on its way out.
        McpError::Connection(message) => McpError::Connection(message.clone().into()),
        McpError::Handshake(message) => McpError::Handshake(message.clone()),
        McpError::OAuth(message) => McpError::OAuth(message.clone()),
        McpError::Internal(message) => McpError::Internal(message.clone()),
        McpError::Result(error) => McpError::Result(error.clone()),
        other => McpError::Connection(
            other
                .source_diagnostic()
                .unwrap_or_else(|| other.to_string().into()),
        ),
    }
}

/// Whether a rejected streamable `initialize` POST means "this server speaks
/// legacy HTTP+SSE" rather than "this server said no".
///
/// Upstream `Hs`, verbatim (2.1.267, present identically in both chunks that
/// carry the fallback):
///
/// ```js
/// function Hs(e){
///   if(!(e instanceof o_) || (e.status!==400 && e.status!==404 && e.status!==405)) return !1;
///   let n = e.data.text;
///   if(typeof n !== "string") return !0;
///   return !js(n);
/// }
/// ```
///
/// 🚨 **Both halves are load-bearing.** The status alone is not evidence: a
/// server that answers the rejected POST with a valid JSON-RPC error has a real
/// protocol failure, and re-dialling it would turn protocol errors into silent
/// transport churn. Only a rejection whose body is *not* a JSON-RPC message
/// says "you are speaking the wrong protocol at me".
///
/// A body we never captured maps to upstream's `typeof n !== "string"` arm and
/// therefore falls back — a transport that cannot supply one does not get to
/// veto the rescue.
fn rejection_invites_legacy_sse(http: &HandshakeHttp) -> bool {
    if !matches!(http.status, 400 | 404 | 405) {
        return false;
    }
    if http.body.trim().is_empty() {
        return true;
    }
    !body_is_jsonrpc(&http.body)
}

/// Upstream `js`: read the body as a JSON-RPC message, tolerating SSE framing.
///
/// ```js
/// function js(e){ let n = e.split(/\r?\n/).find((r)=>r.startsWith("data:"));
///                 let o = Rt(n===void 0 ? e : n.slice(5), !1); return Ane(o) }
/// ```
///
/// The `data:` hop matters: a server can reject the POST with an SSE-framed
/// error, and that is still a JSON-RPC answer rather than a wrong-protocol
/// signal.
fn body_is_jsonrpc(body: &str) -> bool {
    let payload = body
        .lines()
        .find(|line| line.starts_with("data:"))
        .map_or(body, |line| &line[5..]);
    mcp::decode_native_mcp_frame(payload.trim().as_bytes()).is_ok()
}

/// The HTTP detail behind a failed handshake.
///
/// [`McpError::HttpResponse`] deliberately carries only `status` and
/// `www_authenticate`; widening that public type would touch every construction
/// and match site across the workspace. The legacy-SSE fallback is the only
/// reader that needs the body, and it lives in this file, so the detail is
/// carried privately from the one place that still has it.
#[derive(Debug, Clone)]
struct HandshakeHttp {
    status: u16,
    body: String,
}

fn handshake_http(error: &ConnectionError) -> Option<HandshakeHttp> {
    let ConnectionError::Router(RouterError::Remote(remote)) = error else {
        return None;
    };
    let status = http_status(&remote.data)?;
    let data = remote.data.as_ref().and_then(projection_from_payload_ref)?;
    let body = data.string_at("/body").unwrap_or_default().to_owned();
    Some(HandshakeHttp { status, body })
}

fn handshake_error(error: &ConnectionError) -> McpError {
    if let ConnectionError::Router(RouterError::Remote(remote)) = error {
        if let Some(status) = http_status(&remote.data) {
            return McpError::HttpResponse {
                status,
                www_authenticate: remote
                    .data
                    .as_ref()
                    .and_then(projection_from_payload_ref)
                    .and_then(|data| data.string_at("/wwwAuthenticate").map(str::to_owned)),
            };
        }
    }
    McpError::Handshake(error.to_string().into())
}

#[derive(Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<RawTool>,
}

#[derive(Deserialize)]
struct RawTool {
    name: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    description: Option<Utf16JsonProjection>,
    #[serde(
        rename = "inputSchema",
        default = "empty_source_object",
        with = "lingxi_core::types::utf16_json::raw_json_serde"
    )]
    input_schema: Utf16JsonProjection,
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    output_schema: Option<Utf16JsonProjection>,
    #[serde(default)]
    annotations: Option<McpToolAnnotationsDto>,
    #[serde(default)]
    icons: Vec<McpIconDto>,
    #[serde(
        default,
        rename = "_meta",
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    meta: Option<Utf16JsonProjection>,
}

impl RawTool {
    fn into_dto(self) -> McpToolDto {
        let search_hint = self.meta.as_ref().and_then(|meta| {
            let view = meta.get_property("anthropic/searchHint")?;
            view.as_str()?;
            meta.subprojection("/anthropic~1searchHint").ok()
        });
        let always_load = self
            .meta
            .as_ref()
            .and_then(|meta| meta.get_property("anthropic/alwaysLoad"))
            .and_then(|value| value.as_bool());
        let requires_user_interaction = self
            .meta
            .as_ref()
            .and_then(|meta| meta.get_property("anthropic/requiresUserInteraction"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        McpToolDto {
            server_name: String::new(),
            tool_name: self.name.clone(),
            description: self.description.unwrap_or_else(|| source_string("")),
            input_schema: self.input_schema,
            output_schema: self.output_schema,
            annotations: self.annotations,
            icons: self.icons,
            meta: self.meta,
            full_name: format!("mcp____{}", self.name),
            search_hint,
            always_load,
            requires_user_interaction,
        }
    }
}

#[derive(Deserialize)]
struct ToolCallResult {
    #[serde(
        default = "null_source_value",
        with = "lingxi_core::types::utf16_json::raw_json_serde"
    )]
    content: Utf16JsonProjection,
    #[serde(rename = "isError", default)]
    is_error: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    error: Option<Utf16JsonProjection>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    meta: Option<Utf16JsonProjection>,
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    structured_content: Option<Utf16JsonProjection>,
}

#[derive(Deserialize)]
struct ResourcesListResult {
    #[serde(default)]
    resources: Vec<RawResource>,
}

#[derive(Deserialize)]
struct RawResource {
    #[serde(deserialize_with = "deserialize_required_source_string")]
    uri: Utf16JsonProjection,
    #[serde(deserialize_with = "deserialize_required_source_string")]
    name: Utf16JsonProjection,
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    description: Option<Utf16JsonProjection>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    meta: Option<Utf16JsonProjection>,
}

#[derive(Deserialize)]
struct ResourceTemplatesListResult {
    #[serde(default, rename = "resourceTemplates")]
    resource_templates: Vec<RawResourceTemplate>,
}

#[derive(Deserialize)]
struct RawResourceTemplate {
    #[serde(
        rename = "uriTemplate",
        deserialize_with = "deserialize_required_source_string"
    )]
    uri_template: Utf16JsonProjection,
    #[serde(deserialize_with = "deserialize_required_source_string")]
    name: Utf16JsonProjection,
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    description: Option<Utf16JsonProjection>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    annotations: Option<Utf16JsonProjection>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    meta: Option<Utf16JsonProjection>,
}

#[derive(Deserialize)]
struct ResourceReadResult {
    #[serde(default)]
    contents: Vec<RawResourceContent>,
}

#[derive(Deserialize)]
struct RawResourceContent {
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    uri: Option<Utf16JsonProjection>,
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    text: Option<Utf16JsonProjection>,
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    blob: Option<Utf16JsonProjection>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    meta: Option<Utf16JsonProjection>,
}

#[derive(Deserialize)]
struct PromptsListResult {
    #[serde(default)]
    prompts: Vec<RawPrompt>,
}

#[derive(Deserialize)]
struct RawPrompt {
    #[serde(deserialize_with = "deserialize_required_source_string")]
    name: Utf16JsonProjection,
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    description: Option<Utf16JsonProjection>,
    #[serde(default)]
    arguments: Vec<RawPromptArgument>,
}

#[derive(Deserialize)]
struct RawPromptArgument {
    #[serde(deserialize_with = "deserialize_required_source_string")]
    name: Utf16JsonProjection,
    #[serde(default, deserialize_with = "deserialize_optional_source_string")]
    description: Option<Utf16JsonProjection>,
    #[serde(default)]
    required: bool,
}
