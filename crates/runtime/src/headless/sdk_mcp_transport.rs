//! SDK-hosted MCP connections over the headless control plane.
//!
//! Platform connections remain owned by the platform delegate. SDK connections
//! use the existing JSON-RPC connection/client/registry lifecycle; the only
//! changed boundary is delivery of one JSON-RPC message as `mcp_message`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use lingxi_core::host::*;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use lingxi_core::types::McpConnectionId;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::control_plane::StdioControlPlane;

struct Endpoint {
    server: String,
    inbound: mpsc::UnboundedSender<jsonrpc::Message>,
    connection: OnceLock<Arc<jsonrpc::Connection>>,
    client: OnceLock<Arc<::mcp::McpClient>>,
    closed: CancellationToken,
    metadata: Mutex<Option<McpServerMetadataDto>>,
    manifest: Mutex<Option<Manifest>>,
}

struct Manifest {
    initialize: Option<Utf16JsonProjection>,
    tools: Option<Utf16JsonProjection>,
    swallow_initialized: bool,
}

/// Decorates the current platform MCP services with the native SDK lane.
pub struct SdkMcpTransport {
    delegate: crate::desktop::DesktopMcpServices,
    plane: Arc<StdioControlPlane>,
    cwd: PathBuf,
    endpoints: Mutex<HashMap<McpConnectionId, Arc<Endpoint>>>,
    manifests: Mutex<HashMap<String, Manifest>>,
}

impl Drop for SdkMcpTransport {
    fn drop(&mut self) {
        for endpoint in self
            .endpoints
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, endpoint)| endpoint)
        {
            endpoint.closed.cancel();
            if let Some(connection) = endpoint.connection.get() {
                connection.close();
            }
        }
    }
}

impl SdkMcpTransport {
    pub fn new(
        delegate: crate::desktop::DesktopMcpServices,
        plane: Arc<StdioControlPlane>,
        cwd: PathBuf,
    ) -> Arc<Self> {
        let transport = Arc::new(Self {
            delegate,
            plane: plane.clone(),
            cwd,
            endpoints: Mutex::new(HashMap::new()),
            manifests: Mutex::new(HashMap::new()),
        });
        plane.set_sdk_mcp_transport(Arc::downgrade(&transport));
        transport
    }

    fn endpoint(&self, id: McpConnectionId) -> Option<Arc<Endpoint>> {
        self.endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Deliver a server's own SDK JSON-RPC request/notification/response.
    pub fn deliver(&self, server: &str, message: &Utf16JsonProjection) -> Result<(), String> {
        let endpoint = self
            .endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|endpoint| endpoint.server == server)
            .cloned()
            .ok_or_else(|| format!("MCP server {server} is not connected"))?;
        let message = jsonrpc_message_from_projection(message)
            .map_err(|error| format!("Invalid MCP message: {error}"))?;
        endpoint
            .inbound
            .send(message)
            .map_err(|_| "MCP connection is closed".into())
    }

    /// Park one-shot handshake manifests for the connect after initialize.
    /// Names absent from sdkMcpServers are ignored, as in native 2.1.293.
    pub fn park_manifests(
        &self,
        names: &[String],
        projection: Option<&Utf16JsonProjection>,
    ) -> Option<Value> {
        let projection = projection?;
        if projection
            .at_pointer("")
            .is_some_and(JsonValueView::is_null)
        {
            return None;
        }
        let connected: Vec<_> = self
            .endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|endpoint| {
                !endpoint.closed.is_cancelled()
                    && endpoint
                        .metadata
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
            })
            .map(|endpoint| endpoint.server.clone())
            .collect();
        let mut statuses = serde_json::Map::new();
        if projection.object_entries().is_none() {
            for name in names {
                statuses.insert(name.clone(), json!("malformed"));
            }
            return Some(Value::Object(statuses));
        }
        let mut manifests = self
            .manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in names {
            let Some(entry) = projection.get_property(name) else {
                continue;
            };
            let initialize = entry
                .get_property("initializeResult")
                .filter(|value| value.object_entries().is_some());
            let tools = entry
                .get_property("toolsListResult")
                .filter(|value| !value.is_null());
            let status = if initialize.is_none()
                || tools.is_some_and(|value| value.object_entries().is_none())
            {
                "malformed"
            } else if connected.contains(name) {
                "already_connected"
            } else if initialize
                .and_then(|value| value.get_property("protocolVersion"))
                .and_then(|value| value.as_str())
                != Some(::mcp::initialize_params::LATEST_PROTOCOL_VERSION)
            {
                "protocol_version_mismatch"
            } else {
                manifests.insert(
                    name.clone(),
                    Manifest {
                        initialize: projection
                            .subprojection(&format!(
                                "/{}/initializeResult",
                                name.replace('~', "~0").replace('/', "~1")
                            ))
                            .ok(),
                        tools: tools.and_then(|_| {
                            projection
                                .subprojection(&format!(
                                    "/{}/toolsListResult",
                                    name.replace('~', "~0").replace('/', "~1")
                                ))
                                .ok()
                        }),
                        swallow_initialized: false,
                    },
                );
                "parked"
            };
            statuses.insert(name.clone(), json!(status));
        }
        Some(Value::Object(statuses))
    }
}

fn closed() -> jsonrpc::ConnectionError {
    jsonrpc::ConnectionError::Router(jsonrpc::RouterError::WriterClosed)
}

fn jsonrpc_message_from_projection(
    projection: &Utf16JsonProjection,
) -> Result<jsonrpc::Message, serde_json::Error> {
    let source = projection
        .to_json_string()
        .map_err(|error| <serde_json::Error as serde::de::Error>::custom(error.to_string()))?;
    serde_json::from_str(&source)
}

fn projection_from_jsonrpc_message(
    message: &jsonrpc::Message,
) -> Result<Utf16JsonProjection, jsonrpc::ConnectionError> {
    let source = serde_json::to_string(message).map_err(jsonrpc::RouterError::Serialize)?;
    Utf16JsonProjection::parse(&source).map_err(projection_error)
}

fn projection_from_jsonrpc_payload(payload: &jsonrpc::JsonPayload) -> Option<Utf16JsonProjection> {
    Utf16JsonProjection::parse(payload.as_json()).ok()
}

fn capture_initialize(endpoint: &Endpoint, result: &Utf16JsonProjection) {
    *endpoint
        .metadata
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(McpServerMetadataDto {
        raw_capabilities: result.subprojection("/capabilities").ok(),
        server_info: result.subprojection("/serverInfo").ok(),
        instructions: result
            .string_at("/instructions")
            .and_then(|_| result.subprojection("/instructions").ok()),
        ..Default::default()
    });
}

enum Replay {
    Forward,
    Swallow,
    Response(jsonrpc::Response),
}
fn replay_manifest(endpoint: &Endpoint, message: &jsonrpc::Message) -> Replay {
    let mut manifest = endpoint
        .manifest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(cache) = manifest.as_mut() else {
        return Replay::Forward;
    };
    match message {
        jsonrpc::Message::Request(request) if request.method == "initialize" => {
            let Some(result) = cache.initialize.take() else {
                *manifest = None;
                return Replay::Forward;
            };
            let params = request
                .params
                .as_ref()
                .and_then(projection_from_jsonrpc_payload);
            if params
                .as_ref()
                .and_then(|params| params.string_at("/protocolVersion"))
                != result.string_at("/protocolVersion")
                || !params.as_ref().is_some_and(has_sdk_initialize_capabilities)
                || !valid_manifest_result(&result)
            {
                *manifest = None;
                return Replay::Forward;
            }
            cache.swallow_initialized = true;
            capture_initialize(endpoint, &result);
            let result_json = result
                .to_json_string()
                .expect("validated manifest is valid JSON");
            Replay::Response(
                jsonrpc::Response::success_raw_json(request.id.clone(), result_json)
                    .expect("validated manifest projection"),
            )
        }
        jsonrpc::Message::Notification(notification)
            if notification.method == "notifications/initialized" && cache.swallow_initialized =>
        {
            cache.swallow_initialized = false;
            Replay::Swallow
        }
        jsonrpc::Message::Request(request)
            if request.method == "tools/list"
                && !cache.swallow_initialized
                && !request
                    .params
                    .as_ref()
                    .and_then(projection_from_jsonrpc_payload)
                    .is_some_and(|params| params.contains_property("cursor")) =>
        {
            match cache.tools.take() {
                Some(result) if valid_manifest_result(&result) => {
                    let result_json = result
                        .to_json_string()
                        .expect("validated tools manifest is valid JSON");
                    Replay::Response(
                        jsonrpc::Response::success_raw_json(request.id.clone(), result_json)
                            .expect("validated manifest projection"),
                    )
                }
                _ => {
                    *manifest = None;
                    Replay::Forward
                }
            }
        }
        _ => {
            *manifest = None;
            Replay::Forward
        }
    }
}

fn valid_manifest_result(value: &Utf16JsonProjection) -> bool {
    let Some(meta) = value.get_property("_meta") else {
        return true;
    };
    if meta.object_entries().is_none() {
        return false;
    }
    if meta
        .get_property("progressToken")
        .is_some_and(|value| value.as_str().is_none() && value.as_number().is_none())
    {
        return false;
    }
    meta.get_property("io.modelcontextprotocol/related-task")
        .is_none_or(|task| {
            task.object_entries().is_some()
                && task
                    .get_property("taskId")
                    .is_some_and(|task_id| task_id.as_str().is_some())
        })
}

fn has_sdk_initialize_capabilities(params: &Utf16JsonProjection) -> bool {
    let Some(capabilities) = params.get_property("capabilities") else {
        return false;
    };
    if !capabilities
        .object_entries()
        .is_some_and(|entries| entries.len() == 2)
    {
        return false;
    }
    let roots = capabilities.get_property("roots");
    let elicitation = capabilities.get_property("elicitation");
    roots.is_some_and(|roots| {
        roots
            .object_entries()
            .is_some_and(|entries| entries.len() == 1)
            && roots
                .get_property("listChanged")
                .is_some_and(|list_changed| list_changed.as_bool() == Some(true))
    }) && elicitation.is_some_and(|elicitation| {
        elicitation
            .object_entries()
            .is_some_and(|entries| entries.is_empty())
    })
}

struct EndpointConnectGuard {
    endpoint: Arc<Endpoint>,
    disarmed: bool,
}
impl Drop for EndpointConnectGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.endpoint.closed.cancel();
            if let Some(connection) = self.endpoint.connection.get() {
                connection.close();
            }
        }
    }
}
struct SdkConnectGuard<'a> {
    transport: &'a SdkMcpTransport,
    id: McpConnectionId,
    disarmed: bool,
}
impl Drop for SdkConnectGuard<'_> {
    fn drop(&mut self) {
        if !self.disarmed {
            self.transport.disconnect_sync(self.id);
        }
    }
}

async fn send_message(
    endpoint: &Endpoint,
    plane: &StdioControlPlane,
    message: jsonrpc::Message,
) -> Result<(), jsonrpc::ConnectionError> {
    match replay_manifest(endpoint, &message) {
        Replay::Response(response) => {
            return endpoint
                .inbound
                .send(jsonrpc::Message::Response(response))
                .map_err(|_| closed())
        }
        Replay::Swallow => return Ok(()),
        Replay::Forward => {}
    }
    let initialize =
        matches!(&message, jsonrpc::Message::Request(request) if request.method=="initialize");
    let wire = projection_from_jsonrpc_message(&message)?;
    let request = Utf16JsonProjection::object(vec![
        ("subtype".into(), "mcp_message".into()),
        ("server_name".into(), endpoint.server.as_str().into()),
        ("message".into(), wire),
    ])
    .expect("MCP control request has unique fields");
    let (id, response) = plane.send_request(request, None).await;
    let request_id = Utf16JsonProjection::from(id.as_str());
    let payload = tokio::select! {
        result = tokio::time::timeout(Duration::from_millis(70_000), response) => match result {
            Ok(Ok(Ok(payload))) => payload,
            _ => {plane.cancel_request(&request_id).await;return Err(closed());}
        },
        () = endpoint.closed.cancelled() => {plane.cancel_request(&request_id).await;return Err(closed());}
    };
    if payload.contains_property("mcp_response") {
        if initialize {
            if let Ok(reply) = payload.subprojection("/mcp_response") {
                if let Ok(result) = reply.subprojection("/result") {
                    capture_initialize(endpoint, &result);
                }
            }
        }
        let reply_source = payload
            .subprojection("/mcp_response")
            .map_err(projection_error)?;
        let reply = jsonrpc_message_from_projection(&reply_source).map_err(|error| {
            jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Deserialize(error))
        })?;
        endpoint.inbound.send(reply).map_err(|_| closed())?;
    }
    Ok(())
}

fn projection_error(
    error: lingxi_core::types::utf16_json::Utf16JsonProjectionError,
) -> jsonrpc::ConnectionError {
    let error = <serde_json::Error as serde::de::Error>::custom(error.to_string());
    jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Deserialize(error))
}

fn sdk_error(error: impl std::fmt::Display) -> McpError {
    McpError::Connection(error.to_string().into())
}

#[async_trait]
impl McpTransport for SdkMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let McpTransportSpec::SdkControl {
            control_channel_id: server,
        } = spec
        else {
            return self.delegate.transport.connect(spec).await;
        };
        let (inbound, receiver) = mpsc::unbounded_channel();
        let endpoint = Arc::new(Endpoint {
            server: server.clone(),
            inbound,
            connection: OnceLock::new(),
            client: OnceLock::new(),
            closed: CancellationToken::new(),
            metadata: Mutex::new(None),
            manifest: Mutex::new(
                self.manifests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(server),
            ),
        });
        let reader = tokio_stream::wrappers::UnboundedReceiverStream::new(receiver);
        let sender = endpoint.clone();
        let plane = self.plane.clone();
        let writer = Box::pin(futures::sink::unfold(
            (sender, plane),
            |(endpoint, plane), message| async move {
                send_message(&endpoint, &plane, message).await?;
                Ok((endpoint, plane))
            },
        ));
        let connection = Arc::new(jsonrpc::Connection::from_message_streams(reader, writer));
        endpoint
            .connection
            .set(connection.clone())
            .map_err(|_| sdk_error("duplicate SDK connection"))?;
        let mut guard = EndpointConnectGuard {
            endpoint: endpoint.clone(),
            disarmed: false,
        };
        let client = Arc::new(
            ::mcp::McpClient::new(server.clone(), self.cwd.clone(), connection)
                .await
                .with_transport_kind(McpTransportKind::SdkControl),
        );
        endpoint
            .client
            .set(client)
            .map_err(|_| sdk_error("duplicate SDK client"))?;
        let connection_id = McpConnectionId::new();
        self.endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(connection_id, endpoint);
        guard.disarmed = true;
        Ok(McpRawConnection { connection_id })
    }
    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        if !matches!(spec, McpTransportSpec::SdkControl { .. }) {
            return self
                .delegate
                .transport
                .connect_and_initialize(spec, options)
                .await;
        }
        let connection = self.connect(spec).await?;
        let mut guard = SdkConnectGuard {
            transport: self,
            id: connection.connection_id,
            disarmed: false,
        };
        let capabilities = match tokio::time::timeout(
            Duration::from_millis(options.deadline_ms),
            self.initialize(&connection),
        )
        .await
        {
            Ok(Ok(capabilities)) => capabilities,
            result => {
                return Err(sdk_error(match result {
                    Ok(Err(error)) => error.to_string(),
                    _ => "MCP connection deadline exceeded".into(),
                }));
            }
        };
        guard.disarmed = true;
        Ok(McpConnectResult {
            connection,
            capabilities,
            negotiated: McpNegotiatedProtocol {
                era: McpProtocolEra::Legacy,
                version: ::mcp::initialize_params::LATEST_PROTOCOL_VERSION.into(),
            },
        })
    }
    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        let Some(endpoint) = self.endpoint(conn.connection_id) else {
            return self.delegate.transport.initialize(conn).await;
        };
        let capabilities = endpoint
            .client
            .get()
            .expect("initialized client handle")
            .initialize()
            .await
            .map_err(sdk_error)?;
        endpoint
            .connection
            .get()
            .expect("connection handle")
            .notify("notifications/initialized", json!({}))
            .map_err(sdk_error)?;
        Ok(capabilities)
    }
    fn server_metadata(&self, id: McpConnectionId) -> Option<McpServerMetadataDto> {
        self.endpoint(id)
            .and_then(|endpoint| {
                endpoint
                    .metadata
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .or_else(|| self.delegate.transport.server_metadata(id))
    }
    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .list_tools()
                .await
                .map_err(sdk_error),
            None => self.delegate.transport.list_tools(conn).await,
        }
    }
    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .list_resources()
                .await
                .map_err(sdk_error),
            None => self.delegate.transport.list_resources(conn).await,
        }
    }
    async fn list_resource_templates(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        let Some(endpoint) = self.endpoint(conn.connection_id) else {
            return self.delegate.transport.list_resource_templates(conn).await;
        };
        let response = endpoint
            .connection
            .get()
            .expect("connection")
            .call_raw_json("resources/templates/list", "{}".to_owned())
            .await
            .map_err(sdk_error)?;
        let result = Utf16JsonProjection::parse(&response).map_err(sdk_error)?;
        let templates = match result.subprojection("/resourceTemplates") {
            Ok(templates) => templates,
            Err(_) => Utf16JsonProjection::parse("[]").expect("empty array is valid JSON"),
        };
        let templates = templates.to_json_string().map_err(sdk_error)?;
        serde_json::from_str(&templates).map_err(sdk_error)
    }
    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .list_prompts()
                .await
                .map_err(sdk_error),
            None => self.delegate.transport.list_prompts(conn).await,
        }
    }
    async fn call_tool(
        &self,
        conn: &McpRawConnection,
        tool: &str,
        input: Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .call_tool(
                    &format!(
                        "mcp__{}__{tool}",
                        ::mcp::normalization::normalize_name_for_mcp(&endpoint.server)
                    ),
                    input,
                )
                .await
                .map_err(sdk_error),
            None => self.delegate.transport.call_tool(conn, tool, input).await,
        }
    }
    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .read_resource(uri)
                .await
                .map_err(sdk_error),
            None => self.delegate.transport.read_resource(conn, uri).await,
        }
    }
    async fn read_resource_rich(
        &self,
        conn: &McpRawConnection,
        uri: &Utf16JsonProjection,
        output: &std::path::Path,
    ) -> Result<Vec<McpResourceContentsRich>, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .read_resource_rich(uri, output)
                .await
                .map_err(sdk_error),
            None => {
                self.delegate
                    .transport
                    .read_resource_rich(conn, uri, output)
                    .await
            }
        }
    }
    async fn ping(&self, id: McpConnectionId) -> Result<(), McpError> {
        match self.endpoint(id) {
            Some(endpoint) => endpoint
                .client
                .get()
                .expect("client")
                .ping()
                .await
                .map_err(sdk_error),
            None => self.delegate.transport.ping(id).await,
        }
    }
    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        match self.endpoint(conn.connection_id) {
            Some(endpoint) => Ok(endpoint
                .client
                .get()
                .expect("client")
                .subscribe_notifications()),
            None => self.delegate.transport.notifications(conn).await,
        }
    }
    async fn handle_elicitation(
        &self,
        conn: &McpRawConnection,
        request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        if self.endpoint(conn.connection_id).is_some() {
            Ok(ElicitResultDto {
                data: Utf16JsonProjection::parse(r#"{"action":"cancel"}"#)
                    .expect("default-deny elicitation result is valid JSON"),
            })
        } else {
            self.delegate
                .transport
                .handle_elicitation(conn, request)
                .await
        }
    }
    async fn disconnect(&self, id: McpConnectionId) -> Result<(), McpError> {
        if self.endpoint(id).is_some() {
            self.disconnect_sync(id);
            Ok(())
        } else {
            self.delegate.transport.disconnect(id).await
        }
    }
    fn disconnect_sync(&self, id: McpConnectionId) {
        if let Some(endpoint) = self
            .endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
        {
            endpoint.closed.cancel();
            if let Some(connection) = endpoint.connection.get() {
                connection.close();
            }
        } else {
            self.delegate.transport.disconnect_sync(id);
        }
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        let mut transports = self.delegate.transport.supported_transports();
        if !transports.contains(&McpTransportKind::SdkControl) {
            transports.push(McpTransportKind::SdkControl);
        }
        transports
    }
}

impl ::mcp::RawConnectionProvider for SdkMcpTransport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
        self.endpoint(id)
            .and_then(|endpoint| endpoint.connection.get().cloned())
            .or_else(|| self.delegate.raw_connections.connection_for(id))
    }
}

/// Parse native initialize SDK server declarations with the current MCP schema.
pub fn sdk_servers_from_initialize(frame: &Utf16JsonProjection) -> Vec<::mcp::McpServerConfig> {
    let Some(name_count) = frame.array_len("/request/sdkMcpServers") else {
        return Vec::new();
    };
    let mut servers = serde_json::Map::new();
    for index in 0..name_count {
        let name_pointer = format!("/request/sdkMcpServers/{index}");
        let Some(name) = frame
            .string_units(&name_pointer)
            .and_then(|units| String::from_utf16(&units).ok())
        else {
            continue;
        };
        let mut config = json!({"type":"sdk","name":name});
        let settings = frame
            .subprojection("/request/sdkMcpServerConfigs")
            .ok()
            .and_then(|configs| {
                let units = name.encode_utf16().collect::<Vec<_>>();
                configs.subprojection_property_units(&units).ok()
            });
        if let Some(settings) = settings.filter(|settings| settings.object_entries().is_some()) {
            for key in ["timeout", "disableAutoBackground"] {
                if key == "timeout" {
                    let timeout = projection_u64_at(&settings, "/timeout");
                    if let Some(timeout) = timeout.filter(|timeout| *timeout > 0) {
                        config[key] = json!(timeout);
                    }
                } else if settings
                    .at_pointer("/disableAutoBackground")
                    .and_then(|value| value.as_bool())
                    == Some(true)
                {
                    config[key] = json!(true);
                }
            }
        }
        servers.insert(name.to_owned(), config);
    }
    ::mcp::json_config::parse_mcp_json_string(
        &Value::Object(servers).to_string(),
        ::mcp::ConfigScope::Dynamic,
    )
    .unwrap_or_default()
}

fn projection_u64_at(projection: &Utf16JsonProjection, pointer: &str) -> Option<u64> {
    let number = projection.number_at(pointer)?;
    if !number.is_finite()
        || number.fract() != 0.0
        || !(0.0..18_446_744_073_709_551_616.0).contains(&number)
    {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(number as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headless::stream_json::OutboundMsg;

    fn endpoint() -> (Endpoint, mpsc::UnboundedReceiver<jsonrpc::Message>) {
        let (inbound, receiver) = mpsc::unbounded_channel();
        (
            Endpoint {
                server: "fixture".into(),
                inbound,
                connection: OnceLock::new(),
                client: OnceLock::new(),
                closed: CancellationToken::new(),
                metadata: Mutex::new(None),
                manifest: Mutex::new(None),
            },
            receiver,
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_manifest_connect_uses_existing_client_and_registry_transport_lifecycle() {
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let platform = Arc::new(platform_posix::PosixMcpTransport::new());
        let transport = SdkMcpTransport::new(
            crate::desktop::DesktopMcpServices {
                transport: platform.clone(),
                raw_connections: platform,
            },
            plane,
            PathBuf::from("/tmp"),
        );
        let manifest = Utf16JsonProjection::parse(r#"{"fixture":{"initializeResult":{"protocolVersion":"2025-11-25","serverInfo":{"name":"fixture","version":"1"},"capabilities":{"tools":{"listChanged":true}}},"toolsListResult":{"tools":[{"name":"exact","inputSchema":{"type":"object","properties":{"\ud800":{"enum":["\ud801"]}}}}]}}}"#).unwrap();
        assert_eq!(
            transport
                .park_manifests(&["fixture".into()], Some(&manifest))
                .unwrap()["fixture"],
            "parked"
        );
        let connection = transport
            .connect_and_initialize(
                &McpTransportSpec::SdkControl {
                    control_channel_id: "fixture".into(),
                },
                McpConnectOptions {
                    deadline_ms: 1_000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let tools = transport.list_tools(&connection.connection).await.unwrap();
        assert_eq!(tools.len(), 1);
        let schema = tools[0].input_schema_projection.as_ref().unwrap();
        assert_eq!(
            schema
                .subprojection("/properties")
                .unwrap()
                .object_entries()
                .unwrap()[0]
                .code_units,
            vec![0xd800]
        );
        assert!(schema.to_json_string().unwrap().contains(r#""\ud801""#));
        assert!(
            outbound.try_recv().is_err(),
            "manifest handshake and first list are local replay"
        );
        let endpoint = transport
            .endpoint(connection.connection.connection_id)
            .unwrap();
        transport
            .disconnect(connection.connection.connection_id)
            .await
            .unwrap();
        assert!(endpoint.closed.is_cancelled());
        assert!(transport
            .endpoint(connection.connection.connection_id)
            .is_none());
    }

    #[tokio::test]
    async fn sdk_roundtrip_preserves_jsonrpc_ids_keys_and_content_on_shared_plane() {
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let (endpoint, mut inbound) = endpoint();
        let endpoint = Arc::new(endpoint);
        let source = Utf16JsonProjection::parse(r#"{"jsonrpc":"2.0","id":"\ud800","method":"tools/call","params":{"name":"exact","arguments":{"\ud801":"\udfff"}}}"#).unwrap();
        let message = jsonrpc_message_from_projection(&source).unwrap();
        let task_endpoint = endpoint.clone();
        let task_plane = plane.clone();
        let pending =
            tokio::spawn(async move { send_message(&task_endpoint, &task_plane, message).await });
        let OutboundMsg::Line(line) = outbound.recv().await.unwrap() else {
            panic!("SDK control request")
        };
        let request = Utf16JsonProjection::parse(&line).unwrap();
        assert_eq!(
            request.string_units("/request/message/id"),
            Some(vec![0xd800])
        );
        let arguments = request
            .subprojection("/request/message/params/arguments")
            .unwrap();
        assert_eq!(
            arguments.object_entries().unwrap()[0].code_units,
            vec![0xd801]
        );
        let mut response = Utf16JsonProjection::parse(r#"{"type":"control_response","response":{"subtype":"success","response":{"mcp_response":{"jsonrpc":"2.0","id":"\ud800","result":{"content":[{"type":"text","text":"\udfff"}],"\ud801":true}}}}}"#).unwrap();
        let mut envelope = response.subprojection("/response").unwrap();
        envelope
            .set_field("request_id", request.subprojection("/request_id").unwrap())
            .unwrap();
        response.set_field("response", envelope).unwrap();
        plane.resolve_response(&response).await;
        pending.await.unwrap().unwrap();
        let received = projection_from_jsonrpc_message(&inbound.recv().await.unwrap()).unwrap();
        assert_eq!(received.string_units("/id"), Some(vec![0xd800]));
        assert_eq!(
            received.string_units("/result/content/0/text"),
            Some(vec![0xdfff])
        );
        assert_eq!(
            received
                .subprojection("/result")
                .unwrap()
                .object_entries()
                .unwrap()[1]
                .code_units,
            vec![0xd801]
        );
    }

    #[tokio::test]
    async fn sdk_close_cancels_outstanding_native_control_request() {
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let (endpoint, _inbound) = endpoint();
        let endpoint = Arc::new(endpoint);
        let task_endpoint = endpoint.clone();
        let task_plane = plane.clone();
        let pending = tokio::spawn(async move {
            send_message(
                &task_endpoint,
                &task_plane,
                jsonrpc::Message::Request(jsonrpc::Request::new(
                    "tools/list",
                    None,
                    jsonrpc::Id::Number(1),
                )),
            )
            .await
        });
        let OutboundMsg::Line(request) = outbound.recv().await.unwrap() else {
            panic!("request")
        };
        let request: Value = serde_json::from_str(&request).unwrap();
        endpoint.closed.cancel();
        assert!(pending.await.unwrap().is_err());
        let OutboundMsg::Line(cancel) = outbound.recv().await.unwrap() else {
            panic!("cancel")
        };
        let cancel: Value = serde_json::from_str(&cancel).unwrap();
        assert_eq!(cancel["type"], "control_cancel_request");
        assert_eq!(cancel["request_id"], request["request_id"]);
    }

    #[test]
    fn manifest_replay_is_one_shot_and_keeps_exact_catalog_schema() {
        let (endpoint, _receiver) = endpoint();
        let result = Utf16JsonProjection::parse(r#"{"tools":[{"name":"exact","inputSchema":{"type":"object","properties":{"\ud800":{"enum":["\ud801"]}}}}]}"#).unwrap();
        *endpoint.manifest.lock().unwrap() = Some(Manifest {
            initialize: None,
            tools: Some(result.clone()),
            swallow_initialized: false,
        });
        let request = jsonrpc::Message::Request(jsonrpc::Request::new(
            "tools/list",
            None,
            jsonrpc::Id::Number(7),
        ));
        let Replay::Response(response) = replay_manifest(&endpoint, &request) else {
            panic!("manifest")
        };
        let response_result =
            Utf16JsonProjection::parse(response.result.as_ref().unwrap().as_json()).unwrap();
        assert_eq!(
            response_result.to_json_string().unwrap(),
            result.to_json_string().unwrap()
        );
        assert!(matches!(
            replay_manifest(&endpoint, &request),
            Replay::Forward
        ));
    }
}
