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
    drive_modern_request, JsonrpcMcpResultIo, McpInputRequiredOptions, McpResultError,
};
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError, McpIconDto,
    McpNegotiatedProtocol, McpNotificationDto, McpNotificationStream, McpPromptDto, McpProtocolEra,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpResourceTemplateDto,
    McpServerMetadataDto, McpToolAnnotationsDto, McpToolDto, McpToolResultDto, McpTransport,
    McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use serde::Deserialize;
use serde_json::{json, Value};
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

fn bounded_probe_timeout_ms(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(MAX_PROBE_TIMEOUT_MS)
        .min(MAX_PROBE_TIMEOUT_MS)
}

/// Shared remote HTTP/SSE MCP transport.
#[derive(Default)]
pub struct RemoteMcpTransport {
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
            .ok_or_else(|| McpError::Internal(format!("missing MCP capability authority for {id}")))
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
        params: Value,
    ) -> Result<Value, McpError> {
        let Some(protocol) = self.negotiated_for(id) else {
            return Ok(params);
        };
        if protocol.era == McpProtocolEra::Modern && modern_request_requires_meta(method) {
            let mut object = params.as_object().cloned().unwrap_or_default();
            object.insert(
                "_meta".into(),
                modern_meta(&protocol.version, self.elicitation_for(id)?.modern),
            );
            Ok(Value::Object(object))
        } else {
            Ok(params)
        }
    }

    async fn call_rpc(
        &self,
        conn: &McpRawConnection,
        method: &str,
        params: Value,
    ) -> Result<Value, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id))
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
                    )["io.modelcontextprotocol/clientCapabilities"]
                        .clone(),
                },
                method,
                params,
                McpInputRequiredOptions::default(),
            )
            .await
            .map_err(map_result_err)
        } else {
            connection
                .call(method, params)
                .await
                .map_err(|error| map_call_err(&error))
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
            McpError::Connection(format!("no such connection: {}", conn.connection_id))
        })?;
        let mut corrective_retry = false;
        loop {
            let timeout = Self::remaining(deadline)
                .ok_or_else(|| McpError::Connection("MCP connection deadline exceeded".into()))?
                .min(probe_timeout);
            match connection
                .call_with_timeout_probe::<Value, Value>(
                    "server/discover",
                    modern_probe_params(self.elicitation_for(conn.connection_id)?.modern),
                    timeout,
                )
                .await
            {
                Ok(reply) => return Ok(parse_modern_discovery(reply)),
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
                McpError::Connection(format!("no such connection: {}", conn.connection_id)),
                None,
            )
        })?;
        let result: Value = connection
            .call_with_timeout(
                "initialize",
                self.decorate_params(
                    conn.connection_id,
                    "initialize",
                    initialize_params_for_version(
                        version,
                        self.elicitation_for(conn.connection_id)
                            .map_err(|error| (error, None))?
                            .legacy,
                    ),
                )
                .map_err(|error| (error, None))?,
                timeout,
            )
            .await
            .map_err(|error| (handshake_error(&error), handshake_http(&error)))?;
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
            .map_err(|error| (McpError::Handshake(error.to_string()), None))?;
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
            crate::connect_sse(
                url,
                None,
                headers,
                crate::mcp_sse::SseEndpointMode::LegacyRescue {
                    post_method_not_allowed,
                },
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
            McpTransportSpec::Sse { url, headers, .. } => crate::connect_sse(
                url,
                None,
                headers,
                crate::mcp_sse::SseEndpointMode::EndpointEvent,
            )
            .await
            .map_err(McpError::from)?,
            McpTransportSpec::SseIde {
                url, auth_token, ..
            } => {
                let headers = lingxi_core::host::McpHeaders::default();
                // The IDE serves both directions on one url and sends no
                // `endpoint` event.
                crate::connect_sse(
                    url,
                    auth_token.as_deref(),
                    &headers,
                    crate::mcp_sse::SseEndpointMode::SameUrl,
                )
                .await
                .map_err(McpError::from)?
            }
            McpTransportSpec::WsIde {
                url, auth_token, ..
            } => {
                let url = url
                    .parse::<url::Url>()
                    .map_err(|error| McpError::Connection(error.to_string()))?;
                crate::mcp_ws::connect_ws_optional(url, auth_token.as_deref())
                    .await
                    .map_err(|error| McpError::Connection(error.to_string()))?
            }
            McpTransportSpec::Http { url, headers, .. } => {
                crate::connect_http(url, None, headers, Some(Duration::from_secs(60)))
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
        let raw = self.call_rpc(conn, "tools/list", json!({})).await?;
        let parsed: ToolsListResult = serde_json::from_value(raw).map_err(internal_decode)?;
        Ok(parsed.tools.into_iter().map(RawTool::into_dto).collect())
    }

    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        let raw = self.call_rpc(conn, "resources/list", json!({})).await?;
        let parsed: ResourcesListResult = serde_json::from_value(raw).map_err(internal_decode)?;
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
            .call_rpc(conn, "resources/templates/list", json!({}))
            .await?;
        let parsed: ResourceTemplatesListResult =
            serde_json::from_value(raw).map_err(internal_decode)?;
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
        let raw = self.call_rpc(conn, "prompts/list", json!({})).await?;
        let parsed: PromptsListResult = serde_json::from_value(raw).map_err(internal_decode)?;
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
        input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id))
        })?;
        let timeout = Duration::from_secs(60);
        let params = self.decorate_params(
            conn.connection_id,
            "tools/call",
            json!({"name":tool,"arguments":input}),
        )?;
        let raw: Value = if self
            .negotiated_for(conn.connection_id)
            .is_some_and(|p| p.era == McpProtocolEra::Modern)
        {
            drive_modern_request(
                &JsonrpcMcpResultIo {
                    connection,
                    client_capabilities: modern_meta(
                        MODERN_PROTOCOL_VERSION,
                        self.elicitation_for(conn.connection_id)?.modern,
                    )["io.modelcontextprotocol/clientCapabilities"]
                        .clone(),
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
            connection
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
                })?
        };
        let parsed: ToolCallResult = serde_json::from_value(raw).map_err(internal_decode)?;
        Ok(McpToolResultDto {
            result_projection: None,

            content: parsed.content,
            is_error: parsed.is_error,
            meta: parsed.meta,
            structured_content: parsed.structured_content,
        })
    }

    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        let raw = self
            .call_rpc(conn, "resources/read", json!({ "uri": uri }))
            .await?;
        let parsed: ResourceReadResult = serde_json::from_value(raw).map_err(internal_decode)?;
        let first = parsed
            .contents
            .into_iter()
            .next()
            .ok_or_else(|| McpError::Internal("resources/read returned no contents".into()))?;
        Ok(McpResourceContentDto {
            uri: first.uri.unwrap_or_else(|| uri.to_string()),
            content: first.text.or(first.blob).unwrap_or_default(),
            mime_type: first.mime_type,
            meta: first.meta,
        })
    }

    async fn ping(&self, conn_id: lingxi_core::types::McpConnectionId) -> Result<(), McpError> {
        let _: Value = self
            .call_rpc(
                &McpRawConnection {
                    connection_id: conn_id,
                },
                "ping",
                json!({}),
            )
            .await?;
        Ok(())
    }

    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        let connection = self.connection_for(conn.connection_id).ok_or_else(|| {
            McpError::Connection(format!("no such connection: {}", conn.connection_id))
        })?;
        let stream =
            futures::stream::unfold(connection.notifications(), |mut receiver| async move {
                loop {
                    match receiver.recv().await {
                        Ok(notification) => {
                            return Some((
                                McpNotificationDto {
                                    method: notification.method,
                                    params: notification.params.unwrap_or(Value::Null),
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

/// Decode the modern discovery dispatch schema (2.1.286 `Wc`/`Ao`).
/// `resultType` is an opaque dispatch field here; unlike a catalog result,
/// discovery does not require it. Invalid discovery is legacy evidence.
pub fn parse_modern_discovery(mut reply: Value) -> Option<ModernDiscovery> {
    let object = reply.as_object_mut()?;
    let versions = object.get("supportedVersions")?.as_array()?;
    if !versions.iter().all(Value::is_string)
        || !versions
            .iter()
            .any(|version| version == MODERN_PROTOCOL_VERSION)
        || object
            .get("instructions")
            .is_some_and(|value| !value.is_string())
    {
        return None;
    }
    let capabilities = normalize_capabilities(object.get("capabilities")?)?;
    object.insert("capabilities".into(), capabilities.clone());
    let server_info = if let Some(meta) = object.get_mut("_meta") {
        let meta = meta.as_object_mut()?;
        let info = meta.get(SERVER_INFO_KEY).and_then(normalize_server_info);
        meta.remove(SERVER_INFO_KEY);
        if let Some(info) = &info {
            meta.insert(SERVER_INFO_KEY.into(), info.clone());
        }
        info
    } else {
        None
    };
    let ttl_ms = object
        .get("ttlMs")
        .and_then(Value::as_f64)
        .filter(|ttl| *ttl >= 0.0 && ttl.fract() == 0.0 && *ttl <= 9_007_199_254_740_991.0)
        .and_then(|ttl| ttl.to_string().parse::<u64>().ok())
        .unwrap_or(0);
    object.insert("ttlMs".into(), json!(ttl_ms));
    if !matches!(
        object.get("cacheScope").and_then(Value::as_str),
        Some("public" | "private")
    ) {
        object.insert("cacheScope".into(), json!("private"));
    }
    let instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ModernDiscovery {
        version: MODERN_PROTOCOL_VERSION.to_string(),
        capabilities: capabilities_from_wire(capabilities.as_object()?, Some(&capabilities)),
        metadata: McpServerMetadataDto {
            server_info,
            instructions,
            raw_capabilities: Some(capabilities),
            discovery: Some(reply),
        },
    })
}

fn normalize_server_info(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    source.get("name")?.as_str()?;
    source.get("version")?.as_str()?;
    let mut info = serde_json::Map::new();
    for key in ["name", "version", "title", "description", "websiteUrl"] {
        if let Some(value) = source.get(key) {
            value.as_str()?;
            info.insert(key.into(), value.clone());
        }
    }
    if let Some(icons) = source.get("icons") {
        let icons = icons
            .as_array()?
            .iter()
            .map(|icon| {
                let source = icon.as_object()?;
                source.get("src")?.as_str()?;
                let mut icon = serde_json::Map::new();
                for key in ["src", "mimeType", "theme"] {
                    if let Some(value) = source.get(key) {
                        let text = value.as_str()?;
                        if key == "theme" && !matches!(text, "light" | "dark") {
                            return None;
                        }
                        icon.insert(key.into(), value.clone());
                    }
                }
                if let Some(sizes) = source.get("sizes") {
                    if !sizes.as_array()?.iter().all(Value::is_string) {
                        return None;
                    }
                    icon.insert("sizes".into(), sizes.clone());
                }
                Some(Value::Object(icon))
            })
            .collect::<Option<Vec<_>>>()?;
        info.insert("icons".into(), Value::Array(icons));
    }
    Some(Value::Object(info))
}

fn normalize_capabilities(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let mut result = serde_json::Map::new();
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
        let Some(value) = source.get(key) else {
            continue;
        };
        if key == "tasks" {
            continue;
        }
        let object = value.as_object()?;
        let normalized = match key {
            "tools" | "resources" | "prompts" => {
                let mut caps = serde_json::Map::new();
                for field in ["listChanged", "subscribe"] {
                    if field == "subscribe" && key != "resources" {
                        continue;
                    }
                    if let Some(value) = object.get(field) {
                        value.as_bool()?;
                        caps.insert(field.into(), value.clone());
                    }
                }
                Value::Object(caps)
            }
            "extensions" | "experimental" => {
                if !object.values().all(Value::is_object) {
                    return None;
                }
                value.clone()
            }
            _ => value.clone(),
        };
        result.insert(key.into(), normalized);
    }
    Some(Value::Object(result))
}

/// Validate the server-selected legacy revision before acknowledging initialization.
pub fn parse_legacy_initialize(
    reply: Value,
) -> Result<(ServerCapabilitiesDto, String, McpServerMetadataDto), McpError> {
    let version = reply
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            McpError::Handshake("initialize result missing `protocolVersion` string".into())
        })?;
    if !LEGACY_PROTOCOL_VERSIONS.contains(&version) {
        return Err(McpError::Handshake(format!(
            "Server's protocol version is not supported: {version}"
        )));
    }
    let caps = reply
        .get("capabilities")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| {
            McpError::Handshake("initialize result missing or invalid `capabilities` object".into())
        })?;
    let server_info = reply
        .get("serverInfo")
        .and_then(normalize_server_info)
        .ok_or_else(|| {
            McpError::Handshake("initialize result missing or invalid `serverInfo`".into())
        })?;
    if reply
        .get("instructions")
        .is_some_and(|value| !value.is_string())
    {
        return Err(McpError::Handshake(
            "initialize result invalid `instructions`".into(),
        ));
    }
    Ok((
        capabilities_from_wire(caps.as_object().expect("validated object"), Some(&caps)),
        version.to_string(),
        McpServerMetadataDto {
            server_info: Some(server_info),
            raw_capabilities: Some(caps),
            instructions: reply
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_string),
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
    if error
        .data
        .as_ref()
        .and_then(|data| data.get("probeTransportFailure"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Err(McpError::Connection(format!(
            "Version negotiation probe failed: {}",
            error.message
        )));
    }
    // HTTP rejections carry a bounded response body through the HTTP adapter.
    // If that body is itself a JSON-RPC error, use its version evidence.
    if error
        .data
        .as_ref()
        .and_then(|data| data.get("httpStatus"))
        .is_some()
    {
        if let Some(body_error) = error
            .data
            .as_ref()
            .and_then(|data| data.get("body"))
            .and_then(Value::as_str)
            .and_then(|body| serde_json::from_str::<Value>(body).ok())
            .and_then(|body| body.get("error").cloned())
            .and_then(|error| {
                Some(jsonrpc::ResponseError {
                    code: error.get("code")?.as_i64()?.try_into().ok()?,
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    data: error.get("data").cloned(),
                })
            })
        {
            return modern_probe_error(&body_error, retried);
        }
        return Ok(ModernProbeErrorAction::Legacy);
    }
    if error.code != -32022 {
        return Ok(ModernProbeErrorAction::Legacy);
    }
    let Some(supported) = error
        .data
        .as_ref()
        .and_then(|data| data.get("supported"))
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty() && values.iter().all(Value::is_string))
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
    if supported.iter().any(|version| {
        version
            .as_str()
            .is_some_and(|version| version >= MODERN_PROTOCOL_VERSION)
    }) {
        return Err(McpError::Handshake(format!(
            "MCP unsupported protocol version: {}; supported={supported:?}",
            error.message
        )));
    }
    Ok(ModernProbeErrorAction::Legacy)
}

/// Build the protocol-versioned initialize request shared by remote and stdio.
pub fn initialize_params_for_version(
    version: &str,
    elicitation: lingxi_core::host::McpElicitationMode,
) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": lingxi_core::host::mcp_client_capabilities(elicitation),
        "clientInfo": {
            "name": MCP_CLIENT_NAME,
            "title": MCP_CLIENT_TITLE,
            "version": env!("CARGO_PKG_VERSION"),
            "description": CLIENT_DESCRIPTION,
            "websiteUrl": MCP_WEBSITE_URL,
        },
    })
}

/// Build the modern `server/discover` request envelope.
pub fn modern_probe_params(elicitation: lingxi_core::host::McpElicitationMode) -> Value {
    json!({
        "_meta": modern_meta(MODERN_PROTOCOL_VERSION, elicitation),
    })
}

/// Build the closed modern request metadata envelope.
pub fn modern_meta(version: &str, elicitation: lingxi_core::host::McpElicitationMode) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": MCP_CLIENT_NAME,
            "title": MCP_CLIENT_TITLE,
            "version": env!("CARGO_PKG_VERSION"),
            "description": CLIENT_DESCRIPTION,
            "websiteUrl": MCP_WEBSITE_URL,
        },
        "io.modelcontextprotocol/clientCapabilities": lingxi_core::host::mcp_client_capabilities(elicitation),
    })
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
pub fn directory_read_capability(capabilities: Option<&Value>) -> bool {
    capabilities
        .and_then(|value| value.get("extensions"))
        .and_then(|value| value.get(MCP_SKILLS_EXTENSION_KEY))
        .and_then(Value::as_object)
        .and_then(|value| value.get("directoryRead"))
        .and_then(Value::as_bool)
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::{bounded_probe_timeout_ms, RawResourceTemplate, RawTool};

    #[test]
    fn caller_probe_timeout_is_clamped_to_shared_remote_cap() {
        assert_eq!(bounded_probe_timeout_ms(None), 5_000);
        assert_eq!(bounded_probe_timeout_ms(Some(1_000)), 1_000);
        assert_eq!(bounded_probe_timeout_ms(Some(50_000)), 5_000);
    }

    #[test]
    fn tool_retrieval_hints_survive_remote_transport_decode() {
        let raw: RawTool = serde_json::from_value(serde_json::json!({
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
        }))
        .expect("decode raw tool");

        let dto = raw.into_dto();
        assert_eq!(dto.search_hint.as_deref(), Some("records lookup"));
        assert_eq!(dto.always_load, Some(true));
        assert_eq!(
            dto.annotations
                .as_ref()
                .and_then(|annotations| annotations.extra.get("vendor/riskTier")),
            Some(&serde_json::json!("reviewed"))
        );
        assert_eq!(
            dto.icons[0].extra.get("vendor/accent"),
            Some(&serde_json::json!("blue"))
        );
        assert_eq!(
            dto.meta,
            Some(serde_json::json!({
                "anthropic/searchHint": "records lookup",
                "anthropic/alwaysLoad": true,
                "vendor/opaque": { "keep": [1, 2, 3] }
            }))
        );
    }

    #[test]
    fn resource_template_metadata_survives_remote_transport_decode() {
        let raw: RawResourceTemplate = serde_json::from_value(serde_json::json!({
            "uriTemplate": "file:///{path}",
            "name": "file",
            "description": "Workspace file",
            "mimeType": "text/plain",
            "annotations": { "audience": ["assistant"], "vendor/rank": 7 },
            "_meta": { "vendor/template": "opaque" }
        }))
        .expect("decode resource template");

        assert_eq!(raw.annotations.as_ref().unwrap()["vendor/rank"], 7);
        assert_eq!(raw.meta.as_ref().unwrap()["vendor/template"], "opaque");
    }
}

/// Decode the wire capability presence map into the shared DTO.
pub fn capabilities_from_wire(
    capabilities: &serde_json::Map<String, Value>,
    raw_capabilities: Option<&Value>,
) -> ServerCapabilitiesDto {
    ServerCapabilitiesDto {
        tools: capabilities.contains_key("tools"),
        resources: capabilities.contains_key("resources"),
        prompts: capabilities.contains_key("prompts"),
        logging: capabilities.contains_key("logging"),
        directory_read: directory_read_capability(raw_capabilities),
        experimental: capabilities
            .get("experimental")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default(),
        extensions: capabilities
            .get("extensions")
            .and_then(Value::as_object)
            .map(|value| {
                value
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn map_result_err(error: McpResultError) -> McpError {
    match error {
        McpResultError::Connection(error) => map_call_err(&error),
        McpResultError::Sdk(error) => McpError::Result(error),
        McpResultError::Local(error) => McpError::Internal(format!(
            "local handler: code={}, message={}",
            error.code, error.message
        )),
    }
}

fn map_call_err(error: &ConnectionError) -> McpError {
    McpError::Internal(error.to_string())
}

fn internal_decode(error: serde_json::Error) -> McpError {
    McpError::Internal(error.to_string())
}

fn http_status(data: &Option<Value>) -> Option<u16> {
    data.as_ref()
        .and_then(|value| value.get("httpStatus"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
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
        if message.contains("deadline exceeded"));
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
                .to_string(),
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
        McpError::Connection(message) => McpError::Connection(message.clone()),
        McpError::Handshake(message) => McpError::Handshake(message.clone()),
        McpError::OAuth(message) => McpError::OAuth(message.clone()),
        McpError::Result(error) => McpError::Result(error.clone()),
        other => McpError::Connection(other.to_string()),
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
    serde_json::from_str::<jsonrpc::messages::Message>(payload.trim()).is_ok()
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
    let body = remote
        .data
        .as_ref()
        .and_then(|data| data.get("body"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
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
                    .and_then(|data| data.get("wwwAuthenticate"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
        }
    }
    McpError::Handshake(error.to_string())
}

#[derive(Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<RawTool>,
}

#[derive(Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "inputSchema", default)]
    input_schema: Value,
    #[serde(rename = "outputSchema", default)]
    output_schema: Option<Value>,
    #[serde(default)]
    annotations: Option<McpToolAnnotationsDto>,
    #[serde(default)]
    icons: Vec<McpIconDto>,
    #[serde(default, rename = "_meta")]
    meta: Option<Value>,
}

impl RawTool {
    fn into_dto(self) -> McpToolDto {
        let search_hint = self
            .meta
            .as_ref()
            .and_then(|meta| meta.get("anthropic/searchHint"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let always_load = self
            .meta
            .as_ref()
            .and_then(|meta| meta.get("anthropic/alwaysLoad"))
            .and_then(Value::as_bool);
        let requires_user_interaction = self
            .meta
            .as_ref()
            .and_then(|meta| meta.get("anthropic/requiresUserInteraction"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: String::new(),
            tool_name: self.name.clone(),
            description: self.description,
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
    #[serde(default)]
    content: Value,
    #[serde(rename = "isError", default)]
    is_error: bool,
    #[serde(rename = "_meta", default)]
    meta: Option<Value>,
    #[serde(rename = "structuredContent", default)]
    structured_content: Option<Value>,
}

#[derive(Deserialize)]
struct ResourcesListResult {
    #[serde(default)]
    resources: Vec<RawResource>,
}

#[derive(Deserialize)]
struct RawResource {
    uri: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Option<Value>,
}

#[derive(Deserialize)]
struct ResourceTemplatesListResult {
    #[serde(default, rename = "resourceTemplates")]
    resource_templates: Vec<RawResourceTemplate>,
}

#[derive(Deserialize)]
struct RawResourceTemplate {
    #[serde(rename = "uriTemplate")]
    uri_template: String,
    #[serde(default)]
    name: String,
    description: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    annotations: Option<Value>,
    #[serde(rename = "_meta", default)]
    meta: Option<Value>,
}

#[derive(Deserialize)]
struct ResourceReadResult {
    #[serde(default)]
    contents: Vec<RawResourceContent>,
}

#[derive(Deserialize)]
struct RawResourceContent {
    uri: Option<String>,
    text: Option<String>,
    blob: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Option<Value>,
}

#[derive(Deserialize)]
struct PromptsListResult {
    #[serde(default)]
    prompts: Vec<RawPrompt>,
}

#[derive(Deserialize)]
struct RawPrompt {
    name: String,
    description: Option<String>,
    #[serde(default)]
    arguments: Vec<RawPromptArgument>,
}

#[derive(Deserialize)]
struct RawPromptArgument {
    name: String,
    description: Option<String>,
    #[serde(default)]
    required: bool,
}
