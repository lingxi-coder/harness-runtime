//! SDK-hosted MCP connections over the headless control plane.
//!
//! Platform connections remain owned by the platform delegate. SDK connections
//! use the existing JSON-RPC connection/client/registry lifecycle; the only
//! changed boundary is delivery of one JSON-RPC message as `mcp_message`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use lingxi_core::host::*;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use lingxi_core::types::McpConnectionId;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::control_plane::StdioControlPlane;

struct Endpoint {
    server: String,
    authored_name: ::mcp::McpServerName,
    inbound: mpsc::UnboundedSender<jsonrpc::Message>,
    connection: OnceLock<Arc<jsonrpc::Connection>>,
    client: OnceLock<Arc<::mcp::McpClient>>,
    closed: CancellationToken,
    metadata: Mutex<Option<McpServerMetadataDto>>,
    manifest: Mutex<Option<Manifest>>,
    handshake_admitted: AtomicBool,
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
    authored_names: Mutex<HashMap<String, ::mcp::McpServerName>>,
    retired_servers: Mutex<HashSet<String>>,
    handshake_changed: Arc<tokio::sync::Notify>,
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
            authored_names: Mutex::new(HashMap::new()),
            retired_servers: Mutex::new(HashSet::new()),
            handshake_changed: Arc::new(tokio::sync::Notify::new()),
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

    /// The host owns the private transport handle; the wire uses the exact
    /// authored JavaScript server name, including isolated UTF-16 units.
    pub fn configure_server_names(&self, configs: &[::mcp::McpServerConfig]) {
        let mut names = self
            .authored_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for config in configs {
            if let McpTransportSpec::SdkControl { control_channel_id } = &config.spec {
                names.insert(control_channel_id.clone(), config.authored_name());
                self.retired_servers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(control_channel_id);
            }
        }
    }

    /// Retire SDK handshakes before awaiting registry lifecycle locks. A host
    /// may replace a server whose initialize reply has never arrived.
    pub fn retire_server_keys(&self, names: &[String]) {
        let mut retired = self
            .retired_servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retired.extend(names.iter().cloned());
        let endpoints = self
            .endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for endpoint in endpoints
            .values()
            .filter(|endpoint| names.contains(&endpoint.server))
        {
            endpoint.closed.cancel();
            if let Some(connection) = endpoint.connection.get() {
                connection.close();
            }
        }
    }

    pub fn admit_server_name_units(&self, units: &[u16]) {
        let names = self
            .authored_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut retired = self
            .retired_servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, name) in names.iter().filter(|(_, name)| name.utf16_units() == units) {
            let _ = name;
            retired.remove(key);
        }
    }

    /// Native starts control-channel handshakes before publishing initialize
    /// when no replay manifest was parked. Wait only for frame admission, not
    /// for host replies, so the inbound control loop remains able to answer.
    pub async fn wait_for_handshake_admission(&self, names: &[String]) {
        loop {
            let changed = self.handshake_changed.notified();
            if names.iter().all(|name| {
                self.endpoints
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .any(|endpoint| {
                        endpoint.server == *name
                            && (endpoint.handshake_admitted.load(Ordering::Acquire)
                                || endpoint.closed.is_cancelled())
                    })
            }) {
                return;
            }
            changed.await;
        }
    }

    /// Deliver a server's own SDK JSON-RPC request/notification/response.
    pub fn deliver(&self, server: &str, message: &Utf16JsonProjection) -> Result<(), String> {
        self.deliver_units(&server.encode_utf16().collect::<Vec<_>>(), message)
    }

    pub fn deliver_units(
        &self,
        server: &[u16],
        message: &Utf16JsonProjection,
    ) -> Result<(), String> {
        let endpoint = self
            .endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|endpoint| {
                endpoint.authored_name.utf16_units() == server && !endpoint.closed.is_cancelled()
            })
            .cloned();
        let Some(endpoint) = endpoint else {
            // Native acknowledges late/unknown SDK server messages without
            // delivering them into another connection.
            return Ok(());
        };
        let message = match jsonrpc_message_from_projection(message) {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(server = %String::from_utf16_lossy(server), %error, "Ignoring invalid SDK MCP message");
                return Ok(());
            }
        };
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
    ) -> Option<Utf16JsonProjection> {
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
        let authored_names = self
            .authored_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let authored = |name: &String| {
            authored_names
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.into())
        };
        let mut statuses = Vec::new();
        if projection.object_entries().is_none() {
            for name in names {
                statuses.push((authored(name).utf16_units().to_vec(), "malformed".into()));
            }
            return Utf16JsonProjection::object_units(statuses).ok();
        }
        let mut manifests = self
            .manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in names {
            let source_name = authored(name);
            let Ok(entry_source) =
                projection.subprojection_property_units(source_name.utf16_units())
            else {
                continue;
            };
            let entry = entry_source.at_pointer("")?;
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
                        initialize: entry_source.subprojection("/initializeResult").ok(),
                        tools: tools
                            .and_then(|_| entry_source.subprojection("/toolsListResult").ok()),
                        swallow_initialized: false,
                    },
                );
                "parked"
            };
            statuses.push((source_name.utf16_units().to_vec(), status.into()));
        }
        Utf16JsonProjection::object_units(statuses).ok()
    }
}

fn closed() -> jsonrpc::ConnectionError {
    jsonrpc::ConnectionError::Router(jsonrpc::RouterError::WriterClosed)
}

fn jsonrpc_message_from_projection(
    projection: &Utf16JsonProjection,
) -> Result<jsonrpc::Message, jsonrpc::CodecError> {
    let source = projection
        .to_json_string()
        .map_err(|error| jsonrpc::CodecError::MessageDecode(error.to_string()))?;
    ::mcp::decode_native_mcp_frame(source.as_bytes())
}

fn projection_from_jsonrpc_message(
    message: &jsonrpc::Message,
) -> Result<Utf16JsonProjection, jsonrpc::ConnectionError> {
    let source = serde_json::to_string(message).map_err(jsonrpc::RouterError::Serialize)?;
    Utf16JsonProjection::parse(&source).map_err(projection_error)
}

fn sdk_wire_message(
    message: &jsonrpc::Message,
) -> Result<Utf16JsonProjection, jsonrpc::ConnectionError> {
    let source = projection_from_jsonrpc_message(message)?;
    let (order, omit_empty_params): (&[&str], bool) = match message {
        jsonrpc::Message::Request(request) => (
            &["method", "params", "jsonrpc", "id"],
            jsonrpc_method_is(&request.method, "tools/list"),
        ),
        jsonrpc::Message::Response(response) if response.error.is_some() => {
            (&["jsonrpc", "id", "error"], false)
        }
        jsonrpc::Message::Response(_) => (&["result", "jsonrpc", "id"], false),
        jsonrpc::Message::Notification(notification) => (
            &["jsonrpc", "method", "params"],
            jsonrpc_method_is(&notification.method, "notifications/initialized"),
        ),
    };
    let fields = order
        .iter()
        .filter_map(|name| {
            let value = source.subprojection(&format!("/{name}")).ok()?;
            if *name == "params"
                && omit_empty_params
                && value
                    .object_entries()
                    .is_some_and(|entries| entries.is_empty())
            {
                return None;
            }
            Some(((*name).to_owned(), value))
        })
        .collect();
    Utf16JsonProjection::object(fields).map_err(projection_error)
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

fn jsonrpc_method_is(method: &jsonrpc::JsonRpcString, expected: &str) -> bool {
    method
        .code_units()
        .iter()
        .copied()
        .eq(expected.encode_utf16())
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
        jsonrpc::Message::Request(request) if jsonrpc_method_is(&request.method, "initialize") => {
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
            if jsonrpc_method_is(&notification.method, "notifications/initialized")
                && cache.swallow_initialized =>
        {
            cache.swallow_initialized = false;
            Replay::Swallow
        }
        jsonrpc::Message::Request(request)
            if jsonrpc_method_is(&request.method, "tools/list")
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
        .is_some_and(|value| value.code_units().is_none() && value.as_number().is_none())
    {
        return false;
    }
    meta.get_property("io.modelcontextprotocol/related-task")
        .is_none_or(|task| {
            task.object_entries().is_some()
                && task
                    .get_property("taskId")
                    .is_some_and(|task_id| task_id.code_units().is_some())
        })
}

fn has_sdk_initialize_capabilities(params: &Utf16JsonProjection) -> bool {
    params
        .get_property("capabilities")
        .is_some_and(|capabilities| {
            capabilities
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

struct PendingSdkMessage {
    id: String,
    response: oneshot::Receiver<Result<Utf16JsonProjection, String>>,
    initialize: bool,
    bounded: bool,
}

fn sdk_control_deadline(message: &jsonrpc::Message) -> bool {
    // Native Kdr: method + non-null id => a JSON-RPC request. Its caller
    // already owns the MCP deadline; only notifications/responses need the
    // control-plane acknowledgement timeout.
    !matches!(message, jsonrpc::Message::Request(_))
}

async fn prepare_message(
    endpoint: &Endpoint,
    plane: &StdioControlPlane,
    message: jsonrpc::Message,
) -> Result<Option<PendingSdkMessage>, jsonrpc::ConnectionError> {
    match replay_manifest(endpoint, &message) {
        Replay::Response(response) => {
            endpoint
                .inbound
                .send(jsonrpc::Message::Response(response))
                .map_err(|_| closed())?;
            return Ok(None);
        }
        Replay::Swallow => return Ok(None),
        Replay::Forward => {}
    }
    if endpoint.closed.is_cancelled() {
        return Err(closed());
    }
    let initialize = matches!(&message, jsonrpc::Message::Request(request)
        if jsonrpc_method_is(&request.method, "initialize"));
    let bounded = sdk_control_deadline(&message);
    let wire = sdk_wire_message(&message)?;
    let request = Utf16JsonProjection::object(vec![
        ("subtype".into(), "mcp_message".into()),
        (
            "server_name".into(),
            Utf16JsonProjection::root_string(
                endpoint.authored_name.as_str().to_owned(),
                endpoint.authored_name.utf16_units().to_vec(),
            )
            .expect("source MCP name"),
        ),
        ("message".into(), wire),
    ])
    .expect("MCP control request has unique fields");
    let (id, response) = plane.send_request(request, None).await;
    if initialize {
        endpoint.handshake_admitted.store(true, Ordering::Release);
        if let Some(transport) = plane.sdk_mcp_transport() {
            transport.handshake_changed.notify_waiters();
        }
    }
    Ok(Some(PendingSdkMessage {
        id,
        response,
        initialize,
        bounded,
    }))
}

async fn await_control_reply(
    endpoint: &Endpoint,
    plane: &StdioControlPlane,
    pending: PendingSdkMessage,
) -> Result<(), jsonrpc::ConnectionError> {
    let PendingSdkMessage {
        id,
        response,
        initialize,
        bounded,
    } = pending;
    let request_id = Utf16JsonProjection::from(id.as_str());
    let receive = async {
        if bounded {
            tokio::time::timeout(Duration::from_millis(70_000), response)
                .await
                .map_err(|_| ())?
                .map_err(|_| ())?
                .map_err(|_| ())
        } else {
            response.await.map_err(|_| ())?.map_err(|_| ())
        }
    };
    let payload = tokio::select! {
        result = receive => match result {
            Ok(payload) => payload,
            Err(()) => { plane.cancel_request(&request_id).await; return Err(closed()); }
        },
        () = endpoint.closed.cancelled() => {
            plane.retire_request(&request_id).await; return Err(closed());
        }
    };
    if payload.contains_property("mcp_response") {
        let reply_source = payload
            .subprojection("/mcp_response")
            .map_err(projection_error)?;
        let reply = jsonrpc_message_from_projection(&reply_source).map_err(|error| {
            let error = <serde_json::Error as serde::de::Error>::custom(error.to_string());
            jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Deserialize(error))
        })?;
        if initialize {
            if let Ok(result) = reply_source.subprojection("/result") {
                capture_initialize(endpoint, &result);
            }
        }
        endpoint.inbound.send(reply).map_err(|_| closed())?;
    }
    Ok(())
}

async fn queue_message(
    endpoint: Arc<Endpoint>,
    plane: Arc<StdioControlPlane>,
    message: jsonrpc::Message,
) -> Result<(), jsonrpc::ConnectionError> {
    let Some(pending) = prepare_message(&endpoint, &plane, message).await? else {
        return Ok(());
    };
    // Admit frames in writer order, but wait for host replies independently.
    // Otherwise a server-side request (elicitation/ping) can deadlock behind
    // the tool call whose completion depends on that very reply.
    tokio::spawn(async move {
        if await_control_reply(&endpoint, &plane, pending)
            .await
            .is_err()
        {
            endpoint.closed.cancel();
            if let Some(connection) = endpoint.connection.get() {
                connection.close();
            }
        }
    });
    Ok(())
}

#[cfg(test)]
async fn send_message(
    endpoint: &Endpoint,
    plane: &StdioControlPlane,
    message: jsonrpc::Message,
) -> Result<(), jsonrpc::ConnectionError> {
    if let Some(pending) = prepare_message(endpoint, plane, message).await? {
        await_control_reply(endpoint, plane, pending).await?;
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
        if self
            .retired_servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(server)
        {
            return Err(sdk_error("SDK server was retired"));
        }
        let (inbound, receiver) = mpsc::unbounded_channel();
        let endpoint = Arc::new(Endpoint {
            server: server.clone(),
            authored_name: self
                .authored_names
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(server)
                .cloned()
                .unwrap_or_else(|| server.into()),
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
            handshake_admitted: AtomicBool::new(false),
        });
        let reader = tokio_stream::wrappers::UnboundedReceiverStream::new(receiver);
        let sender = endpoint.clone();
        let plane = self.plane.clone();
        let writer = Box::pin(futures::sink::unfold(
            (sender, plane),
            |(endpoint, plane), message| async move {
                queue_message(endpoint.clone(), plane.clone(), message).await?;
                Ok((endpoint, plane))
            },
        ));
        let connection = Arc::new(
            jsonrpc::Connection::from_message_streams_with_initial_request_id(
                reader,
                writer,
                Some("Method not found".into()),
                0,
            ),
        );
        endpoint
            .connection
            .set(connection.clone())
            .map_err(|_| sdk_error("duplicate SDK connection"))?;
        let mut guard = EndpointConnectGuard {
            endpoint: endpoint.clone(),
            disarmed: false,
        };
        let client = Arc::new(
            ::mcp::McpClient::new_sdk_hosted(server.clone(), self.cwd.clone(), connection).await,
        );
        endpoint
            .client
            .set(client)
            .map_err(|_| sdk_error("duplicate SDK client"))?;
        let connection_id = McpConnectionId::new();
        // Retirement and publication share this short synchronous lock. A
        // queued connect cannot install a new endpoint after its host removed
        // the server but before registry teardown acquired the lifecycle lock.
        let retired = self
            .retired_servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if retired.contains(server) {
            return Err(sdk_error("SDK server was retired"));
        }
        self.endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(connection_id, endpoint);
        self.handshake_changed.notify_waiters();
        drop(retired);
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
            .initialize_with_client_capabilities(
                &Utf16JsonProjection::parse("{}").expect("SDK capabilities are an empty object"),
            )
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
    let mut servers = Vec::<::mcp::McpServerConfig>::new();
    for index in 0..name_count {
        let name_pointer = format!("/request/sdkMcpServers/{index}");
        let Some(name) = frame.string_units(&name_pointer) else {
            continue;
        };
        let mut config = json!({"type":"sdk","name":String::from_utf16_lossy(&name)});
        let settings = frame
            .subprojection("/request/sdkMcpServerConfigs")
            .ok()
            .and_then(|configs| configs.subprojection_property_units(&name).ok());
        if let Some(settings) = settings
            .as_ref()
            .filter(|settings| settings.object_entries().is_some())
        {
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
            if let Some(tools) = sdk_initialize_tool_policies(&settings) {
                config["tools"] = tools;
            }
        }
        if let Some(mut config) =
            ::mcp::build_server_from_json_entry_utf16(&name, &config, ::mcp::ConfigScope::Dynamic)
        {
            if let Some(settings) = settings.as_ref() {
                ::mcp::apply_sdk_mcp_settings(&mut config, settings);
            }
            if let Some(existing) = servers
                .iter_mut()
                .find(|entry| entry.name_matches_utf16(&name))
            {
                *existing = config;
            } else {
                servers.push(config);
            }
        }
    }
    ::mcp::finalize_mcp_server_names(&mut servers);
    for config in &mut servers {
        if let McpTransportSpec::SdkControl { control_channel_id } = &mut config.spec {
            *control_channel_id = config.name.clone();
        }
    }
    servers
}

/// Native `Lyt`/`Ie`/`rjn` (2.1.318 @257935226/@257936383/@257934789):
/// initialize ignores non-list policy fields and nameless entries, coerces an
/// unknown policy to always-ask, and ignores SDK organization-cap declarations.
fn sdk_initialize_tool_policies(settings: &Utf16JsonProjection) -> Option<Value> {
    let tools = settings.get_property("tools")?;
    let count = tools.array_len()?;
    let mut policies = Vec::new();
    for index in 0..count {
        let Some(tool) = tools.get_index(index) else {
            continue;
        };
        let Some(name) = tool
            .get_property("name")
            .and_then(JsonValueView::code_units)
            .and_then(|units| String::from_utf16(units).ok())
        else {
            continue;
        };
        let mut policy = json!({"name":name});
        if let Some(value) = tool.get_property("permission_policy") {
            policy["permission_policy"] = json!(match value.as_str() {
                Some("always_allow") => "always_allow",
                Some("always_deny") => "always_deny",
                _ => "always_ask",
            });
        }
        policies.push(policy);
    }
    Some(Value::Array(policies))
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

    #[tokio::test]
    async fn desktop_factory_transport_and_raw_provider_drive_sdk_registry_connection() {
        use crate::desktop::{DesktopConfig, DesktopMcpServices, DesktopSessionComposition};
        use crate::headless::io::Output;
        use crate::headless::session;
        use crate::headless::stream_json::StreamJsonStream;

        let root = tempfile::tempdir().expect("isolated desktop root");
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = DesktopConfig::default();
        config.cwd = cwd.clone();
        config.lingxi_home = root.path().join("home");
        config.isolated_credential_storage = true;
        config.use_noop_permission_gate = true;
        config.composition = Some(DesktopSessionComposition::HeadlessCli);
        let prepared = session::prepare(&mut config, &crate::headless::SessionStart::New, false)
            .await
            .expect("new session preparation");
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
            tokio::io::sink(),
        )));
        let plane = StdioControlPlane::new(stream.outbound_tx());
        let factory_plane = plane.clone();
        let factory_cwd = cwd.clone();
        config.mcp_services_factory = Some(Arc::new(move |base| {
            let transport = SdkMcpTransport::new(base, factory_plane.clone(), factory_cwd.clone());
            DesktopMcpServices {
                transport: transport.clone(),
                raw_connections: transport,
            }
        }));

        let runtime = session::build_prepared(
            config,
            prepared,
            stream.clone(),
            Arc::new(crate::headless::HeadlessPermissionRequestSink),
        )
        .await
        .expect("desktop assembly");
        let transport = plane
            .sdk_mcp_transport()
            .expect("registry must retain the factory's decorated transport");
        let manifest = Utf16JsonProjection::parse(
            r#"{"fixture":{"initializeResult":{"protocolVersion":"2025-11-25","serverInfo":{"name":"fixture","version":"1"},"capabilities":{"tools":{"listChanged":true}}},"toolsListResult":{"tools":[{"name":"factory-backed","inputSchema":{"type":"object"}}]}}}"#,
        )
        .unwrap();
        assert_eq!(
            transport
                .park_manifests(&["fixture".into()], Some(&manifest))
                .unwrap()
                .string_at("/fixture"),
            Some("parked")
        );
        drop(transport); // Only the assembled runtime may retain the decorated transport.
        let initialize = Utf16JsonProjection::parse(
            r#"{"request":{"sdkMcpServers":["fixture"],"sdkMcpServerConfigs":{"fixture":{"timeout":5000,"tools":[{"name":"factory-backed","permission_policy":"always_deny"}]}}}}"#,
        ).unwrap();
        let configs = sdk_servers_from_initialize(&initialize);
        assert_eq!(configs.len(), 1);

        // Policy intake precedes the initialize reply and any transport work.
        runtime
            .orchestrator
            .apply_sdk_mcp_initialize_policies(&configs)
            .await;
        assert!(runtime
            .enforcing_permission_gate
            .as_ref()
            .unwrap()
            .tool_wide_deny_names()
            .await
            .contains(&"mcp__fixture__factory-backed".into()));
        assert!(runtime.mcp_registry.get_config("fixture").await.is_none());
        runtime.orchestrator.connect_sdk_mcp_servers(configs).await;
        let registry_client = runtime
            .mcp_registry
            .ensure_connected_client("fixture")
            .await
            .expect("registry must use the factory's paired raw-connection provider");
        let transport = plane.sdk_mcp_transport().unwrap();
        let endpoint = transport
            .endpoints
            .lock()
            .unwrap()
            .values()
            .find(|endpoint| endpoint.server == "fixture")
            .cloned()
            .unwrap();
        let connection = endpoint.connection.get().unwrap();
        for method in ["roots/list", "elicitation/create"] {
            assert!(
                connection.local_registered_handler(method).await.is_none(),
                "registry SDK wrapper must not reinstall {method}"
            );
        }
        assert_eq!(
            registry_client
                .server_capabilities_raw()
                .await
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"tools":{"listChanged":true}}"#
        );
        drop(transport);
        let gate = runtime.enforcing_permission_gate.as_ref().unwrap();
        assert!(gate
            .tool_wide_deny_names()
            .await
            .contains(&"mcp__fixture__factory-backed".into()));
        gate.apply_permission_updates(&[Utf16JsonProjection::parse(
            r#"{"type":"addRules","destination":"session","behavior":"deny","rules":[{"toolName":"Read"}]}"#,
        ).unwrap()]);

        // Re-initialize an already connected SDK server: first-registration
        // options stand, new ask/deny restrictions accumulate, and an empty
        // policy list cannot remove an earlier initialize restriction.
        let update = Utf16JsonProjection::parse(
            r#"{"request":{"sdkMcpServers":["fixture"],"sdkMcpServerConfigs":{"fixture":{"timeout":9000,"tools":[{"name":"later","permission_policy":"always_deny"}]}}}}"#,
        ).unwrap();
        runtime
            .orchestrator
            .connect_sdk_mcp_servers(sdk_servers_from_initialize(&update))
            .await;
        assert_eq!(
            runtime
                .mcp_registry
                .get_config("fixture")
                .await
                .unwrap()
                .timeout_ms,
            Some(5000)
        );
        let names = gate.tool_wide_deny_names().await;
        assert!(names.contains(&"mcp__fixture__factory-backed".into()));
        assert!(names.contains(&"mcp__fixture__later".into()));
        let empty = Utf16JsonProjection::parse(
            r#"{"request":{"sdkMcpServers":["fixture"],"sdkMcpServerConfigs":{"fixture":{"tools":[]}}}}"#,
        ).unwrap();
        runtime
            .orchestrator
            .connect_sdk_mcp_servers(sdk_servers_from_initialize(&empty))
            .await;
        assert!(gate
            .tool_wide_deny_names()
            .await
            .contains(&"mcp__fixture__later".into()));

        runtime.mcp_registry.disconnect("fixture").await.unwrap();
        ::mcp::tool_policy::synchronize_permission_policies(&runtime.mcp_registry, gate.as_ref())
            .await;
        let names = gate.tool_wide_deny_names().await;
        assert!(!names.iter().any(|name| name.starts_with("mcp__fixture__")));
        assert!(
            names.contains(&"Read".into()),
            "SDK retirement must preserve session rules"
        );

        let shutdown = runtime.session_lifecycle.shutdown_and_drain().await;
        assert!(shutdown.complete, "{:?}", shutdown.errors);
        stream.finish().await.unwrap();
    }

    #[test]
    fn sdk_initialize_tool_policies_follow_native_tolerant_control_schema() {
        let native: Value = serde_json::from_str(include_str!(
            "../../../mcp/tests/fixtures/claude_2_1_318_policy_upstream_urls.json"
        ))
        .unwrap();
        for case in native["sdkInitializePolicies"]["cases"].as_array().unwrap() {
            let settings =
                Utf16JsonProjection::from_serde_value(&json!({"tools":case["input"]})).unwrap();
            assert_eq!(
                sdk_initialize_tool_policies(&settings).unwrap_or(Value::Null),
                case["expected"],
                "{case}"
            );
        }
        let frame = Utf16JsonProjection::parse(r#"{"request":{"sdkMcpServers":["fixture"],"sdkMcpServerConfigs":{"fixture":{"tools":[{"name":"deny","permission_policy":"always_deny","org_max_permission":"blocked"},{"name":"unknown","permission_policy":"invalid"},{"name":"plain"},{"permission_policy":"always_deny"},{"name":7}]}}}}"#).unwrap();
        let configs = sdk_servers_from_initialize(&frame);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].tools.len(), 3);
        assert_eq!(
            configs[0].tools[0].permission_policy,
            Some(McpToolPermissionPolicy::AlwaysDeny)
        );
        assert_eq!(
            configs[0].tools[1].permission_policy,
            Some(McpToolPermissionPolicy::AlwaysAsk)
        );
        assert_eq!(configs[0].tools[2].permission_policy, None);
        assert!(configs[0]
            .tools
            .iter()
            .all(|tool| tool.org_max_permission.is_none()));
        let not_list = Utf16JsonProjection::parse(r#"{"tools":{"name":"ignored"}}"#).unwrap();
        assert_eq!(sdk_initialize_tool_policies(&not_list), None);
    }

    fn endpoint() -> (Endpoint, mpsc::UnboundedReceiver<jsonrpc::Message>) {
        let (inbound, receiver) = mpsc::unbounded_channel();
        (
            Endpoint {
                server: "fixture".into(),
                authored_name: "fixture".into(),
                inbound,
                connection: OnceLock::new(),
                client: OnceLock::new(),
                closed: CancellationToken::new(),
                metadata: Mutex::new(None),
                manifest: Mutex::new(None),
                handshake_admitted: AtomicBool::new(false),
            },
            receiver,
        )
    }

    fn native_fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/mcp_sdk_2_1_322.json")).unwrap()
    }

    #[test]
    fn native_322_control_fixture_matches_sdk_message_source_bytes() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/mcp_sdk_control_2_1_322.json"
        ))
        .unwrap();
        for raw in fixture["nativeMessages"].as_array().unwrap() {
            let raw = raw.as_str().unwrap();
            let source = Utf16JsonProjection::parse(raw).unwrap();
            // These are outbound reference frames. The native MCP ingress
            // decoder intentionally normalizes params/_meta field order;
            // deserialize only the RPC envelope and retain payload source.
            let message: jsonrpc::Message = serde_json::from_str(raw).unwrap();
            assert_eq!(
                sdk_wire_message(&message)
                    .unwrap()
                    .to_json_string()
                    .unwrap(),
                raw
            );
            if source.string_at("/method") == Some("tools/list") {
                let mut with_empty_params = source.clone();
                with_empty_params
                    .set_field("params", Utf16JsonProjection::parse("{}").unwrap())
                    .unwrap();
                let message: jsonrpc::Message =
                    serde_json::from_str(&with_empty_params.to_json_string().unwrap()).unwrap();
                assert_eq!(
                    sdk_wire_message(&message)
                        .unwrap()
                        .to_json_string()
                        .unwrap(),
                    raw,
                    "empty SDK list params must be omitted"
                );
            }
        }
    }

    #[test]
    fn native_322_manifest_replay_and_deadline_fixture_matches() {
        let fixture = native_fixture();
        assert_eq!(fixture["version"], "2.1.322");
        for case in fixture["replayCases"].as_array().unwrap() {
            let (endpoint, _receiver) = endpoint();
            if let Some(manifest) = case["input"].get("manifest") {
                *endpoint.manifest.lock().unwrap() = Some(Manifest {
                    initialize: manifest
                        .get("initializeResult")
                        .map(|value| Utf16JsonProjection::from_serde_value(value).unwrap()),
                    tools: manifest
                        .get("toolsListResult")
                        .map(|value| Utf16JsonProjection::from_serde_value(value).unwrap()),
                    swallow_initialized: false,
                });
            }
            for (message, expected) in case["input"]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .zip(case["outputs"].as_array().unwrap())
            {
                let source = Utf16JsonProjection::from_serde_value(message).unwrap();
                let actual = match replay_manifest(
                    &endpoint,
                    &jsonrpc_message_from_projection(&source).unwrap(),
                ) {
                    Replay::Forward => json!({"forward":true}),
                    Replay::Swallow => json!({"swallow":true}),
                    Replay::Response(response) => {
                        json!({"reply":serde_json::to_value(response).unwrap()})
                    }
                };
                assert_eq!(&actual, expected, "{}: {message}", case["input"]["name"]);
            }
        }
        for case in fixture["deadlineCases"].as_array().unwrap() {
            let source = Utf16JsonProjection::from_serde_value(&case["message"]).unwrap();
            assert_eq!(
                sdk_control_deadline(&jsonrpc_message_from_projection(&source).unwrap()),
                case["bounded"].as_bool().unwrap(),
                "{}",
                case["message"]
            );
        }
        for case in fixture["utf16MetaCases"].as_array().unwrap() {
            let result = Utf16JsonProjection::parse(&format!(
                r#"{{"_meta":{}}}"#,
                case["metaJson"].as_str().unwrap()
            ))
            .unwrap();
            assert_eq!(
                valid_manifest_result(&result),
                case["accepted"].as_bool().unwrap(),
                "{}",
                case["metaJson"]
            );
        }
    }

    #[tokio::test]
    async fn sdk_outbound_reply_is_admitted_before_earlier_request_settles() {
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let (endpoint, _inbound) = endpoint();
        let endpoint = Arc::new(endpoint);
        for value in native_fixture()["concurrency"]["entered"]
            .as_array()
            .unwrap()
        {
            let source = Utf16JsonProjection::from_serde_value(value).unwrap();
            tokio::time::timeout(
                Duration::from_secs(2),
                queue_message(
                    endpoint.clone(),
                    plane.clone(),
                    jsonrpc_message_from_projection(&source).unwrap(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
        }
        let mut methods = Vec::new();
        for _ in 0..2 {
            let OutboundMsg::Line(line) =
                tokio::time::timeout(Duration::from_secs(2), outbound.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("request")
            };
            let request: Value = serde_json::from_str(&line).unwrap();
            methods.push(request["request"]["message"].clone());
        }
        assert_eq!(
            methods,
            native_fixture()["concurrency"]["entered"]
                .as_array()
                .unwrap()
                .clone()
        );
        assert!(
            !endpoint.closed.is_cancelled(),
            "neither unanswered host reply serializes the writer"
        );
        endpoint.closed.cancel();
        // Native SDK retirement is silent. Both independently owned reply
        // waiters must still complete and release their captured endpoints.
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&endpoint) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both SDK control waiters retire on close");
        assert!(
            outbound.try_recv().is_err(),
            "SDK close emits no outer control_cancel_request"
        );
    }

    #[tokio::test]
    async fn sdk_initialized_notification_omits_empty_params() {
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let (endpoint, _inbound) = endpoint();
        let message = jsonrpc_message_from_projection(
            &Utf16JsonProjection::parse(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let pending = prepare_message(&endpoint, &plane, message)
            .await
            .unwrap()
            .unwrap();
        let OutboundMsg::Line(line) = outbound.recv().await.unwrap() else {
            panic!("request")
        };
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            request["request"]["message"],
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        );
        plane
            .cancel_request(&Utf16JsonProjection::from(pending.id.as_str()))
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_peer_requests_preserve_utf16_ids_and_native_raw_response_order() {
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let platform = Arc::new(platform_posix::PosixMcpTransport::new());
        let transport = SdkMcpTransport::new(
            crate::desktop::DesktopMcpServices {
                transport: platform.clone(),
                raw_connections: platform,
            },
            plane.clone(),
            PathBuf::from("/tmp"),
        );
        let connection = transport
            .connect(&McpTransportSpec::SdkControl {
                control_channel_id: "fixture".into(),
            })
            .await
            .unwrap();
        transport
            .deliver("fixture", &Utf16JsonProjection::parse("{}").unwrap())
            .unwrap();
        transport
            .deliver(
                "missing",
                &Utf16JsonProjection::parse(r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#).unwrap(),
            )
            .unwrap();
        assert!(
            outbound.try_recv().is_err(),
            "invalid and unknown messages are ignored"
        );
        for method in [
            "roots/list",
            "elicitation/create",
            "fixture/unknown",
            "ping",
        ] {
            let input = Utf16JsonProjection::parse(&format!(
                r#"{{"jsonrpc":"2.0","id":"peer\ud800","method":"{method}"}}"#
            ))
            .unwrap();
            transport.deliver("fixture", &input).unwrap();
            let OutboundMsg::Line(line) =
                tokio::time::timeout(Duration::from_secs(2), outbound.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("MCP response")
            };
            let frame = Utf16JsonProjection::parse(&line).unwrap();
            let message = frame
                .subprojection("/request/message")
                .unwrap()
                .to_json_string()
                .unwrap();
            let expected = if method == "ping" {
                r#"{"result":{},"jsonrpc":"2.0","id":"peer\ud800"}"#
            } else {
                r#"{"jsonrpc":"2.0","id":"peer\ud800","error":{"code":-32601,"message":"Method not found"}}"#
            };
            assert_eq!(message, expected, "{method}");
            let ack = Utf16JsonProjection::from_serde_value(&json!({"type":"control_response","response":{"subtype":"success","request_id":frame.string_at("/request_id").unwrap(),"response":{}}})).unwrap();
            plane.resolve_response(&ack).await;
        }
        transport
            .disconnect(connection.connection_id)
            .await
            .unwrap();
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
                .unwrap()
                .string_at("/fixture"),
            Some("parked")
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
        let schema = &tools[0].input_schema;
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
    async fn sdk_close_retires_outstanding_control_request_without_outer_cancel() {
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
        let _request: Value = serde_json::from_str(&request).unwrap();
        endpoint.closed.cancel();
        assert!(pending.await.unwrap().is_err());
        assert!(
            outbound.try_recv().is_err(),
            "native pending SDK removal does not cancel the host's control request"
        );
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
