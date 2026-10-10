//! MCP transport — POSIX.
//!
//! Supports the `Stdio`, `Sse`, `Http`, `SseIde`, and `WsIde` variants. The
//! generic `WebSocket` variant's low-level `connect_ws` helper is re-exported
//! by M2-02c, but `PosixMcpTransport::connect` does NOT route generic
//! `WebSocket` specs. Other variants (`InProcess`, `SdkControl`) return
//! `McpError::UnsupportedTransport`.
//!
//! M2.02c also lands `spawn_stdio`: a low-level helper that spawns a child
//! MCP server, frames its stdio with NDJSON, drains stderr into a 64 MB
//! ring, and returns a fully-wired `jsonrpc::Connection`. Used by
//! the `Stdio` arm here as well as by callers that want stdio plumbing
//! without going through the trait surface.

use async_trait::async_trait;
use jsonrpc::{
    Connection, ConnectionError, InboundHandler, JsonPayload, Request, Response, RouterError,
};
use lingxi_core::host::mcp_result::{
    connection_error_diagnostic, decode_modern_result, drive_modern_request,
    result_error_diagnostic, DecodedMcpResult, JsonrpcMcpResultIo, McpInputRequiredOptions,
    McpResultError,
};
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError,
    McpNegotiatedProtocol, McpNotificationDto, McpNotificationStream, McpPromptDto, McpProtocolEra,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpResourceTemplateDto,
    McpServerMetadataDto, McpToolDto, McpToolResultDto, McpTransport, McpTransportKind,
    McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::McpConnectionId;
use platform_common::mcp_remote::{
    initialize_params_for_version, modern_meta, modern_probe_error, modern_probe_params,
    modern_request_requires_meta, parse_legacy_initialize, parse_modern_discovery, ModernDiscovery,
    ModernProbeErrorAction, MCP_PROTOCOL_VERSION,
};
use platform_common::mcp_stdio::{StderrRing, StdioConfig};
use platform_common::RemoteMcpTransport;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as AsyncMutex;

/// `clientInfo.description` literal (debranded from claude-code's
/// `"Anthropic's agentic coding tool"`). The canonical
/// `mcp::identity::ClientInfo` model does not (yet) carry a `description`
/// field, so the literal lives here at the posix wire boundary.
#[cfg(test)]
const CLIENT_DESCRIPTION: &str = "An agentic coding tool";
const MCP_SKILLS_EXTENSION_KEY: &str = "io.modelcontextprotocol/skills";

/// Build the `params` object for the MCP `initialize` request.
///
/// Factored out as a free function so the exact wire shape is unit-testable
/// without a live connection. Mirrors claude-code's SDK `Client` construction
/// (`services/mcp/client.ts:985-1002`):
///
/// * `capabilities` advertises roots and the current form/URL elicitation
///   modes. The connection's explicit initialize capability choice is passed
///   separately during the combined handshake.
/// * `clientInfo` carries claude-code's identity literals — reused from the
///   canonical `mcp::identity` constants (`name`, `title`, `websiteUrl`) plus
///   the `description` literal — while `version` stays this build's own
///   product version (we do NOT impersonate claude-code's release number).
#[cfg(test)]
fn initialize_params() -> Utf16JsonProjection {
    initialize_params_for_version(
        MCP_PROTOCOL_VERSION,
        lingxi_core::host::McpElicitationMode::FormAndUrl,
    )
}

fn modern_request_requires_result_type(method: &str) -> bool {
    modern_request_requires_meta(method)
}

fn source_string(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust strings are valid UTF-16")
}

fn empty_source_string() -> Utf16JsonProjection {
    source_string("")
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

fn source_payload(value: &Utf16JsonProjection) -> Result<JsonPayload, McpError> {
    let json = value
        .to_json_string()
        .map_err(|error| McpError::Internal(error.to_string().into()))?;
    JsonPayload::parse(json).map_err(|error| McpError::Internal(error.to_string().into()))
}

fn source_from_payload(value: JsonPayload) -> Result<Utf16JsonProjection, McpError> {
    Utf16JsonProjection::parse(value.as_json())
        .map_err(|error| McpError::Internal(error.to_string().into()))
}

fn source_from_payload_ref(value: &JsonPayload) -> Option<Utf16JsonProjection> {
    Utf16JsonProjection::parse(value.as_json()).ok()
}

fn decode_source<T: serde::de::DeserializeOwned>(
    value: &Utf16JsonProjection,
) -> Result<T, McpError> {
    let json = value
        .to_json_string()
        .map_err(|error| McpError::Internal(error.to_string().into()))?;
    serde_json::from_str(&json).map_err(|error| McpError::Internal(error.to_string().into()))
}

#[cfg(test)]
fn source_from_test_json(value: &serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&value.to_string()).expect("test fixture is valid JSON")
}

/// Per-connection state held by `PosixMcpTransport`.
///
/// Different transports keep slightly different ownership: `Stdio` owns the
/// spawned child so `disconnect` can force-kill it; SSE / HTTP just own the
/// JSON-RPC `Connection` (the underlying `reqwest` tasks live inside the
/// connection's broker).
pub(crate) enum PosixMcpConnection {
    /// `Stdio` connection — owns the JSON-RPC link to the spawned child.
    ///
    /// The child process is owned by the reaper task spawned inside
    /// [`spawn_stdio_with_handles`]. Per-connection teardown REQUIRES the
    /// explicit [`disconnect`](PosixMcpTransport::disconnect) path: it signals
    /// the reaper (via [`reaper_kill`](PosixMcpConnection::Stdio::reaper_kill))
    /// to `child.start_kill()` so even a server that ignores stdin-EOF is
    /// force-terminated, then closes the [`Connection`] to abort the broker.
    ///
    /// A plain `Arc<Connection>` drop with the runtime still alive does NOT
    /// abort the broker, does NOT close the child's stdin, and does NOT trigger
    /// `kill_on_drop` (the reaper task is detached and still owns the `Child`),
    /// so the child would leak. `kill_on_drop(true)` only ever fires on full
    /// runtime shutdown. Always go through `disconnect`.
    Stdio {
        /// Fully-wired JSON-RPC `Connection` over the child's NDJSON stdio,
        /// wrapped in an `Arc` so request methods can cheaply clone a handle
        /// out from under the map lock and `.await` without holding the guard.
        /// (`Connection` itself is not `Clone`.)
        connection: Arc<Connection>,
        /// Shared `StderrRing` populated by the stderr-drain task — held so
        /// `disconnect` (or a future crash path) can snapshot child stderr.
        #[allow(dead_code)]
        stderr: Arc<AsyncMutex<StderrRing>>,
        /// Fires the reaper task's `child.start_kill()` so `disconnect` can
        /// force-terminate a non-cooperative server. `Mutex<Option<…>>`
        /// because the oneshot sender is consumed on the first send.
        reaper_kill: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        /// Completion acknowledgement used before replacing a probe sibling.
        reaper_done: tokio::sync::watch::Receiver<bool>,
    },
}

/// POSIX MCP transport.
///
/// Supports the `Stdio`, `Sse`, `Http`, `SseIde`, and `WsIde` variants. Generic
/// `WebSocket`, `InProcess`, and `SdkControl` specs return
/// `McpError::UnsupportedTransport`. Stdio owns process/reaper state while
/// HTTP, SSE, and IDE request handling is delegated to the shared remote
/// transport.
#[derive(Default)]
pub struct PosixMcpTransport {
    connections: Arc<Mutex<HashMap<McpConnectionId, PosixMcpConnection>>>,
    negotiated: Arc<Mutex<HashMap<McpConnectionId, McpNegotiatedProtocol>>>,
    metadata: Arc<Mutex<HashMap<McpConnectionId, McpServerMetadataDto>>>,
    elicitation:
        Arc<Mutex<HashMap<McpConnectionId, lingxi_core::host::McpElicitationCapabilities>>>,
    /// Shared HTTP/SSE implementation. POSIX only owns process/reaper state;
    /// remote JSON-RPC and protocol negotiation live in platform-common.
    remote: Arc<RemoteMcpTransport>,
}

/// Synchronous teardown guard used by the combined handshake. A caller can
/// cancel or time out while an initialize request is waiting; `Drop` must
/// still close the broker and stop a stdio child because async cleanup cannot
/// be awaited once the future has been dropped.
struct ConnectionCleanupGuard {
    id: McpConnectionId,
    connections: Arc<Mutex<HashMap<McpConnectionId, PosixMcpConnection>>>,
    negotiated: Arc<Mutex<HashMap<McpConnectionId, McpNegotiatedProtocol>>>,
    metadata: Arc<Mutex<HashMap<McpConnectionId, McpServerMetadataDto>>>,
    elicitation:
        Arc<Mutex<HashMap<McpConnectionId, lingxi_core::host::McpElicitationCapabilities>>>,
    connection: Arc<Connection>,
    reaper_kill: Option<Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>>,
    armed: bool,
}

impl ConnectionCleanupGuard {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ConnectionCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(reaper_kill) = &self.reaper_kill {
            if let Ok(mut guard) = reaper_kill.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(());
                }
            }
        }
        self.connection.close();
        if let Ok(mut guard) = self.connections.lock() {
            guard.remove(&self.id);
        }
        if let Ok(mut guard) = self.negotiated.lock() {
            guard.remove(&self.id);
        }
        if let Ok(mut guard) = self.metadata.lock() {
            guard.remove(&self.id);
        }
        if let Ok(mut guard) = self.elicitation.lock() {
            guard.remove(&self.id);
        }
    }
}

impl PosixMcpTransport {
    /// Construct a new `PosixMcpTransport`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, id: McpConnectionId, conn: PosixMcpConnection) {
        // Recover from a poisoned std `Mutex` by silently dropping the
        // insert — the engine will surface the failure on the next call
        // when the connection id misses the map.
        if let Ok(mut guard) = self.connections.lock() {
            guard.insert(id, conn);
        }
    }

    fn elicitation_for(
        &self,
        id: McpConnectionId,
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
        id: McpConnectionId,
        capabilities: lingxi_core::host::McpElicitationCapabilities,
    ) -> Result<(), McpError> {
        self.elicitation
            .lock()
            .map_err(|_| McpError::Internal("MCP capability map poisoned".into()))?
            .insert(id, capabilities);
        Ok(())
    }

    fn negotiated_for(&self, id: McpConnectionId) -> Option<McpNegotiatedProtocol> {
        self.negotiated.lock().ok()?.get(&id).cloned()
    }

    fn is_remote_connection(&self, id: McpConnectionId) -> bool {
        self.remote.connection_for(id).is_some()
    }

    fn decorate_params(
        &self,
        id: McpConnectionId,
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
        let connection = self.connection_for_result(conn.connection_id)?;
        let params = self.decorate_params(conn.connection_id, method, params)?;
        if self.negotiated_for(conn.connection_id).is_some_and(|p| {
            p.era == McpProtocolEra::Modern && modern_request_requires_result_type(method)
        }) {
            drive_modern_request(
                &JsonrpcMcpResultIo {
                    connection,
                    client_capabilities: modern_meta(
                        platform_common::mcp_remote::MODERN_PROTOCOL_VERSION,
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
            let params = source_payload(&params)?;
            let result: JsonPayload = connection
                .call(method, params)
                .await
                .map_err(|error| map_call_err(&error))?;
            source_from_payload(result)
        }
    }

    /// Clone the JSON-RPC [`Connection`] for `id` out of the map, dropping the
    /// std `MutexGuard` before the caller `.await`s.
    ///
    /// `Arc::clone` is cheap, so we clone the `Arc` out under the lock and drop
    /// the std `MutexGuard` before the caller `.await`s (`Connection` itself is
    /// not `Clone` — its `broadcast::Receiver` blocks the derive, which is why
    /// it is wrapped in an `Arc`).
    fn connection_for_result(&self, id: McpConnectionId) -> Result<Arc<Connection>, McpError> {
        let guard = self
            .connections
            .lock()
            .map_err(|_| McpError::Internal("connection map mutex poisoned".into()))?;
        // Every transport variant now stores a fully-wired `Arc<Connection>`,
        // so the request surface is transport-agnostic: clone the handle out
        // and let the caller `.await` after the std `MutexGuard` is dropped.
        // The or-pattern is exhaustive over `Some(_)` for the three variants;
        // adding a new variant without a `Connection` would require an arm.
        match guard.get(&id) {
            Some(PosixMcpConnection::Stdio { connection, .. }) => Ok(Arc::clone(connection)),
            None => self
                .remote
                .connection_for(id)
                .ok_or_else(|| McpError::Connection(format!("no such connection: {id}").into())),
        }
    }

    fn cleanup_guard(&self, id: McpConnectionId) -> Result<ConnectionCleanupGuard, McpError> {
        let (connection, reaper_kill) = {
            let guard = self
                .connections
                .lock()
                .map_err(|_| McpError::Internal("connection map mutex poisoned".into()))?;
            match guard.get(&id) {
                Some(PosixMcpConnection::Stdio {
                    connection,
                    reaper_kill,
                    ..
                }) => (Arc::clone(connection), Some(Arc::clone(reaper_kill))),
                None => {
                    return Err(McpError::Connection(
                        format!("no such connection: {id}").into(),
                    ))
                }
            }
        };
        Ok(ConnectionCleanupGuard {
            id,
            connections: Arc::clone(&self.connections),
            negotiated: Arc::clone(&self.negotiated),
            metadata: Arc::clone(&self.metadata),
            elicitation: Arc::clone(&self.elicitation),
            connection,
            reaper_kill,
            armed: true,
        })
    }

    fn remaining(deadline: tokio::time::Instant) -> Option<Duration> {
        deadline.checked_duration_since(tokio::time::Instant::now())
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
        match tokio::time::timeout(remaining, self.connect(spec)).await {
            Ok(result) => result,
            Err(_) => Err(McpError::Connection(
                "MCP connection deadline exceeded".into(),
            )),
        }
    }

    async fn initialize_before(
        &self,
        conn: &McpRawConnection,
        version: &str,
        deadline: tokio::time::Instant,
    ) -> Result<ServerCapabilitiesDto, (McpError, Option<jsonrpc::ResponseError>)> {
        let Some(remaining) = Self::remaining(deadline) else {
            return Err((
                McpError::Connection("MCP connection deadline exceeded".into()),
                None,
            ));
        };
        tokio::time::timeout(
            remaining,
            self.initialize_with_version_detailed(conn, version, remaining),
        )
        .await
        .map_err(|_| {
            (
                McpError::Connection("MCP connection deadline exceeded".into()),
                None,
            )
        })?
    }

    async fn probe_modern(
        &self,
        conn: &McpRawConnection,
        deadline: tokio::time::Instant,
        probe_timeout: Duration,
    ) -> Result<(Option<ModernDiscovery>, bool), McpError> {
        let rpc = self.connection_for_result(conn.connection_id)?;
        let mut corrective_retry = false;
        loop {
            let Some(timeout) = Self::remaining(deadline) else {
                return Ok((None, true));
            };
            let timeout = timeout.min(probe_timeout);
            let params = source_payload(&modern_probe_params(
                self.elicitation_for(conn.connection_id)?.modern,
            ))?;
            match rpc
                .call_with_timeout_probe_ignoring_unknown_ids::<_, JsonPayload>(
                    "server/discover",
                    params,
                    timeout,
                )
                .await
            {
                Ok(reply) => {
                    return Ok((parse_modern_discovery(source_from_payload(reply)?), false));
                }

                Err(ConnectionError::Router(RouterError::Remote(error))) => {
                    match modern_probe_error(&error, corrective_retry)? {
                        ModernProbeErrorAction::Legacy => return Ok((None, false)),
                        ModernProbeErrorAction::Retry => corrective_retry = true,
                    }
                }
                Err(ConnectionError::Router(
                    RouterError::Deserialize(_) | RouterError::WrongResponseId { .. },
                )) => return Ok((None, false)),
                Err(ConnectionError::Router(RouterError::WriterClosed)) if rpc.is_closed() => {
                    return Ok((None, false))
                }
                Err(ConnectionError::Router(RouterError::Timeout(_))) => return Ok((None, true)),
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
    ) -> Result<ServerCapabilitiesDto, (McpError, Option<jsonrpc::ResponseError>)> {
        let connection = self
            .connection_for_result(conn.connection_id)
            .map_err(|error| (error, None))?;
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
        let params = source_payload(&params).map_err(|error| (error, None))?;
        let result: JsonPayload = connection
            .call_with_timeout("initialize", params, timeout)
            .await
            .map_err(|error| {
                let remote = match &error {
                    ConnectionError::Router(RouterError::Remote(remote)) => Some(remote.clone()),
                    _ => None,
                };
                (handshake_error(&error), remote)
            })?;
        let result = source_from_payload(result).map_err(|error| (error, None))?;

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
        if let Ok(mut map) = self.metadata.lock() {
            map.insert(conn.connection_id, metadata);
        }
        connection
            .notify(
                "notifications/initialized",
                source_payload(&empty_source_object()).map_err(|error| (error, None))?,
            )
            .map_err(|error| {
                (
                    McpError::Handshake(connection_error_diagnostic(&error)),
                    None,
                )
            })?;

        Ok(dto)
    }
}

/// Bridge the privately-owned `Arc<jsonrpc::Connection>` out to the `mcp`
/// crate so `McpRegistry::with_raw_conn` can build a live `McpClient` per
/// connected server (the 4 builtin MCP tools dispatch through it).
///
/// Wraps the inherent `connection_for_result` (which returns a `Result`),
/// mapping a missing connection to `None` per the trait contract. The trait
/// lives in `mcp` (not `traits/`) so it can name `jsonrpc::Connection`; the
/// `posix → mcp → jsonrpc` dep DAG makes this impl legal.
impl mcp::RawConnectionProvider for PosixMcpTransport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<Connection>> {
        self.connection_for_result(id).ok()
    }
}

// ---------------------------------------------------------------------------
// Wire-shape deserialization structs for MCP `*/list` and `*/read` results.
// Field renames bridge the wire's camelCase to our snake_case DTOs.
// ---------------------------------------------------------------------------

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
    annotations: Option<lingxi_core::host::McpToolAnnotationsDto>,
    #[serde(default)]
    icons: Vec<lingxi_core::host::McpIconDto>,
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
    /// Wire entry -> DTO. `server_name`/`full_name` carry the empty `<server>`
    /// token; `McpRegistry::connect` rewrites both once it knows the registry
    /// key.
    fn into_dto(self) -> McpToolDto {
        let search_hint = self.meta.as_ref().and_then(|meta| {
            let value = meta.get_property("anthropic/searchHint")?;
            value.as_str()?;
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
            full_name: format!("mcp____{}", self.name),
            server_name: String::new(),
            tool_name: self.name,
            description: self.description.unwrap_or_else(|| source_string("")),
            input_schema: self.input_schema,
            output_schema: self.output_schema,
            annotations: self.annotations,
            icons: self.icons,
            meta: self.meta,
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
    // Optional, arbitrary-JSON MCP `CallToolResult` members. Parsed as opaque
    // values and forwarded verbatim onto the DTO (no transformation), matching
    // claude-code's `result._meta` / `result.structuredContent` passthrough.
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
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    uri: Utf16JsonProjection,
    #[serde(
        default = "empty_source_string",
        with = "lingxi_core::types::utf16_json::raw_json_serde"
    )]
    name: Utf16JsonProjection,
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
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

/// Wire response for `resources/templates/list` (§26a). Oracle
/// `MYe = yEt.extend({resourceTemplates:H(GGt)})` (2.1.251 Mach-O
/// @167622755) — same envelope shape as `ResourcesListResult` but keyed
/// `resourceTemplates` and carrying `uriTemplate` instead of a concrete `uri`.
#[derive(Deserialize)]
struct ResourceTemplatesListResult {
    #[serde(default, rename = "resourceTemplates")]
    resource_templates: Vec<RawResourceTemplate>,
}

#[derive(Deserialize)]
struct RawResourceTemplate {
    #[serde(
        rename = "uriTemplate",
        with = "lingxi_core::types::utf16_json::raw_json_serde"
    )]
    uri_template: Utf16JsonProjection,
    #[serde(
        default = "empty_source_string",
        with = "lingxi_core::types::utf16_json::raw_json_serde"
    )]
    name: Utf16JsonProjection,
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
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
    #[serde(
        default,
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    uri: Option<Utf16JsonProjection>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    text: Option<Utf16JsonProjection>,
    blob: Option<String>,
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
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    name: Utf16JsonProjection,
    #[serde(
        default,
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    description: Option<Utf16JsonProjection>,
    #[serde(default)]
    arguments: Vec<RawPromptArgument>,
}

#[derive(Deserialize)]
struct RawPromptArgument {
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    name: Utf16JsonProjection,
    #[serde(
        default,
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    description: Option<Utf16JsonProjection>,
    #[serde(default)]
    required: bool,
}

/// Map a [`ConnectionError`] from an outbound call into the generic
/// [`McpError::Internal`] surface (used by every method except the ones with
/// a more specific mapping, e.g. `call_tool`'s timeout / not-found paths).
fn map_result_err(error: McpResultError) -> McpError {
    match error {
        McpResultError::Connection(error) => map_call_err(&error),
        McpResultError::Sdk(error) => McpError::Result(error),
        error @ McpResultError::Local(_) => McpError::Internal(result_error_diagnostic(&error)),
    }
}

fn map_call_err(e: &ConnectionError) -> McpError {
    McpError::Internal(connection_error_diagnostic(e))
}

/// Recover structured HTTP status / `WWW-Authenticate` metadata from a failed
/// `initialize` call, instead of flattening it to a `Handshake(String)`.
///
/// The Streamable HTTP writer task has no synchronous way to fail the
/// `initialize` POST directly (it runs the request/response cycle inside a
/// detached `tokio::spawn`, decoupled from the caller's `.call()` future), so
/// a non-2xx response is turned into a *synthetic* JSON-RPC error response
/// (`mcp_http.rs::http_error_message`) carrying `data: {httpStatus,
/// wwwAuthenticate}` — the same structured pair `McpError::HttpResponse`
/// already carries for a genuine transport-level failure (SSE's pre-flight
/// GET). Unwrap that `data` shape here so callers (401/403 auth
/// classification in `mcp::registry`) can match on `McpError::HttpResponse`
/// uniformly regardless of which transport produced it.
///
/// A real MCP protocol failure (bad params, method not found, a plain
/// `RouterError` with no `data`) has no `httpStatus` in `data` and falls
/// back to the prior `Handshake(e.to_string())` behavior unchanged.
fn handshake_error(e: &ConnectionError) -> McpError {
    if let ConnectionError::Router(RouterError::Remote(re)) = e {
        let data = re.data.as_ref().and_then(source_from_payload_ref);
        if let Some(status) = data
            .as_ref()
            .and_then(|data| data.number_at("/httpStatus"))
            .filter(|status| {
                status.is_finite()
                    && status.fract() == 0.0
                    && *status >= 0.0
                    && *status <= u16::MAX as f64
            })
        {
            let www_authenticate = data
                .as_ref()
                .and_then(|data| data.string_at("/wwwAuthenticate").map(str::to_owned));
            return McpError::HttpResponse {
                status: status as u16,
                www_authenticate,
            };
        }
    }
    McpError::Handshake(connection_error_diagnostic(e))
}

/// True when `e` is a remote JSON-RPC error carrying the
/// `METHOD_NOT_FOUND` (-32601) code — the MCP convention for "unknown tool".
fn is_method_not_found(e: &ConnectionError) -> bool {
    matches!(
        e,
        ConnectionError::Router(RouterError::Remote(re)) if re.code == jsonrpc::METHOD_NOT_FOUND
    )
}

/// Inbound handler answering server-initiated `ping` requests with an empty
/// result object `{}`, as the MCP spec requires (a Rust client must still
/// answer inbound pings even though it declares no special capabilities).
struct PingHandler;

#[async_trait]
impl InboundHandler for PingHandler {
    async fn handle(&self, req: Request) -> Response {
        Response::success(req.id, json!({}))
    }
}

#[async_trait]
impl McpTransport for PosixMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let id = McpConnectionId::new();
        match spec {
            McpTransportSpec::Stdio { command, args, env } => {
                // Build a fully-wired JSON-RPC `Connection` over the child's
                // NDJSON stdio instead of spawning a raw `Command` (the old
                // code dropped the link instead of retaining the live broker).
                // `spawn_stdio_with_handles` sets `kill_on_drop(true)`, frames
                // stdio, drains stderr into a 64 MB ring, and reaps the child.
                let cfg = StdioConfig {
                    cmd: command.clone(),
                    args: args.clone(),
                    env: env.clone(),
                    // `McpTransportSpec::Stdio` carries no cwd field, so the
                    // child inherits the parent's working directory.
                    cwd: None,
                };
                let handles = spawn_stdio_with_handles(cfg)
                    .await
                    .map_err(|e| McpError::Connection(e.to_string().into()))?;
                let connection = Arc::new(handles.connection);
                // A spec-compliant server may send us inbound `ping` requests
                // for keepalive (the SDK does this). With no handler the
                // Dispatcher answers METHOD_NOT_FOUND (-32601), which the
                // server reads as a protocol error and may disconnect. Answer
                // inbound pings with an empty result `{}` per the MCP spec.
                connection
                    .register_handler("ping", Arc::new(PingHandler))
                    .await;
                self.insert(
                    id,
                    PosixMcpConnection::Stdio {
                        connection,
                        stderr: handles.stderr,
                        reaper_kill: Arc::new(Mutex::new(Some(handles.reaper_kill))),
                        reaper_done: handles.reaper_done,
                    },
                );
            }
            McpTransportSpec::Sse { .. }
            | McpTransportSpec::Http { .. }
            | McpTransportSpec::SseIde { .. }
            | McpTransportSpec::WsIde { .. } => {
                // The shared remote-only transport owns all HTTP/SSE wire and
                // JSON-RPC state. POSIX retains only the stdio process/reaper
                // implementation below.
                return self.remote.connect(spec).await;
            }
            other => return Err(McpError::UnsupportedTransport(map_kind(other))),
        }
        if let Err(error) = self.set_elicitation(
            id,
            lingxi_core::host::McpElicitationCapabilities::for_transport(spec.transport_kind()),
        ) {
            self.disconnect_sync(id);
            return Err(error);
        }
        Ok(McpRawConnection { connection_id: id })
    }

    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        mut options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        if options.expected_era.is_none() {
            match mcp::protocol_negotiation::resolve_for_spec(spec, options.deadline_ms) {
                mcp::protocol_negotiation::NegotiationMode::Auto { probe_timeout_ms } => {
                    options.expected_era = Some(McpProtocolEra::Modern);
                    options.probe_timeout_ms = Some(probe_timeout_ms);
                }
                mcp::protocol_negotiation::NegotiationMode::Legacy => {
                    options.expected_era = Some(McpProtocolEra::Legacy)
                }
            }
        }
        if matches!(
            spec,
            McpTransportSpec::Sse { .. }
                | McpTransportSpec::Http { .. }
                | McpTransportSpec::SseIde { .. }
                | McpTransportSpec::WsIde { .. }
        ) {
            return self.remote.connect_and_initialize(spec, options).await;
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(options.deadline_ms))
            .ok_or_else(|| McpError::Connection("MCP connection deadline overflow".into()))?;
        let requested =
            options.expected_era.unwrap_or_else(
                || match mcp::protocol_negotiation::resolve_for_spec(spec, options.deadline_ms) {
                    mcp::protocol_negotiation::NegotiationMode::Auto { .. } => {
                        McpProtocolEra::Modern
                    }
                    mcp::protocol_negotiation::NegotiationMode::Legacy => McpProtocolEra::Legacy,
                },
            );
        let mut discovery = None;
        let mut probe_timed_out = false;
        if requested == McpProtocolEra::Modern {
            // Only stdio probes on a disposable sibling; HTTP is handled by
            // platform-common and retains its original connection.
            let probe_connection = self.connect_before(spec, deadline).await?;
            let probe_guard = self.cleanup_guard(probe_connection.connection_id)?;
            self.set_elicitation(probe_connection.connection_id, options.elicitation)?;
            let probe_cap =
                Duration::from_millis(options.probe_timeout_ms.unwrap_or(3_000).min(3_000));
            (discovery, probe_timed_out) = self
                .probe_modern(&probe_connection, deadline, probe_cap)
                .await?;
            let reaped = self.connections.lock().ok().and_then(|connections| {
                connections
                    .get(&probe_connection.connection_id)
                    .map(|PosixMcpConnection::Stdio { reaper_done, .. }| reaper_done.clone())
            });
            drop(probe_guard);
            if let Some(mut reaped) = reaped {
                // The disposable sibling must exit before a live child can
                // claim the same server resources (ports, files or locks).
                tokio::time::timeout_at(deadline, reaped.wait_for(|done| *done))
                    .await
                    .map_err(|_| {
                        McpError::Connection("MCP probe cleanup deadline exceeded".into())
                    })?
                    .map_err(|_| {
                        McpError::Connection("MCP probe reaper closed before cleanup".into())
                    })?;
            }
        }
        let connection = self.connect_before(spec, deadline).await?;
        let live_guard = self.cleanup_guard(connection.connection_id)?;
        self.set_elicitation(connection.connection_id, options.elicitation)?;
        if discovery.is_none() {
            match self
                .initialize_before(&connection, MCP_PROTOCOL_VERSION, deadline)
                .await
            {
                Ok(capabilities) => {
                    let negotiated = self
                        .negotiated_for(connection.connection_id)
                        .ok_or_else(|| McpError::Handshake("missing negotiated protocol".into()))?;
                    live_guard.disarm();
                    return Ok(McpConnectResult {
                        connection,
                        capabilities,
                        negotiated,
                    });
                }
                Err((error, remote)) if probe_timed_out => {
                    if let Some(remote) = remote.filter(|error| error.code == -32022) {
                        let supported = remote
                            .data
                            .as_ref()
                            .and_then(source_from_payload_ref)
                            .and_then(|data| data.subprojection("/supported").ok())
                            .and_then(|versions| {
                                let length = versions.array_len("")?;
                                if length == 0 {
                                    return None;
                                }
                                (0..length)
                                    .map(|index| {
                                        versions.string_at(&format!("/{index}")).map(str::to_owned)
                                    })
                                    .collect::<Option<Vec<_>>>()
                            });
                        if supported.is_some_and(|versions| {
                            !versions.iter().any(|version| {
                                version == platform_common::mcp_remote::MODERN_PROTOCOL_VERSION
                            })
                        }) {
                            return Err(error);
                        }
                    }
                    // An old server can ignore discovery, while a modern peer
                    // can reject initialize after a slow probe. The upstream
                    // client tries discovery once on the live process then.
                    let recovery_deadline = std::cmp::min(
                        deadline,
                        tokio::time::Instant::now()
                            + Duration::from_millis(
                                options.probe_timeout_ms.unwrap_or(3_000).min(3_000),
                            ),
                    );
                    let rpc = self.connection_for_result(connection.connection_id)?;
                    let timeout = Self::remaining(recovery_deadline).unwrap_or_default();
                    // This is a connected discover, not another raw probe: one
                    // request, a complete-result envelope, no corrective retry.
                    let params = source_payload(&modern_probe_params(options.elicitation.modern))?;
                    discovery = match rpc
                        .call_with_timeout::<_, JsonPayload>("server/discover", params, timeout)
                        .await
                    {
                        Ok(reply) => match source_from_payload(reply).and_then(|reply| {
                            decode_modern_result("server/discover", reply)
                                .map_err(|error| McpError::Internal(error.to_string().into()))
                        }) {
                            Ok(DecodedMcpResult::Complete(reply)) => parse_modern_discovery(reply),
                            _ => None,
                        },
                        _ => None,
                    };
                    if discovery.is_none() {
                        return Err(error);
                    }
                }
                Err((error, _)) => return Err(error),
            }
        }
        let discovery = discovery.expect("modern discovery was checked");
        let negotiated = McpNegotiatedProtocol {
            era: McpProtocolEra::Modern,
            version: discovery.version,
        };
        if let Ok(mut map) = self.negotiated.lock() {
            map.insert(connection.connection_id, negotiated.clone());
        }
        if let Ok(mut map) = self.metadata.lock() {
            map.insert(connection.connection_id, discovery.metadata);
        }
        live_guard.disarm();
        Ok(McpConnectResult {
            connection,
            capabilities: discovery.capabilities,
            negotiated,
        })
    }

    fn server_metadata(&self, id: McpConnectionId) -> Option<McpServerMetadataDto> {
        if self.is_remote_connection(id) {
            return self.remote.server_metadata(id);
        }
        self.metadata.lock().ok()?.get(&id).cloned()
    }

    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.initialize(conn).await;
        }
        if let Some(discovery) = self
            .server_metadata(conn.connection_id)
            .and_then(|metadata| metadata.discovery)
            .and_then(parse_modern_discovery)
        {
            return Ok(discovery.capabilities);
        }
        self.initialize_with_version(conn, MCP_PROTOCOL_VERSION, Duration::from_secs(60))
            .await
    }

    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_tools(conn).await;
        }
        let raw = self
            .call_rpc(conn, "tools/list", empty_source_object())
            .await?;
        let parsed: ToolsListResult = decode_source(&raw)?;

        // The trait's `connect`/`list_tools` carry no logical server name —
        // only an `McpConnectionId`. We leave `server_name` empty (and the
        // `full_name` FQN unprefixed by a server) and let the `lingxi-mcp`
        // layer rewrite the FQN once it knows the registry key.
        Ok(parsed.tools.into_iter().map(RawTool::into_dto).collect())
    }

    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_resources(conn).await;
        }
        let raw = self
            .call_rpc(conn, "resources/list", empty_source_object())
            .await?;
        let parsed: ResourcesListResult = decode_source(&raw)?;
        Ok(parsed
            .resources
            .into_iter()
            .map(|r| McpResourceDto {
                uri: r.uri,
                name: r.name,
                description: r.description,
                mime_type: r.mime_type,
                meta: r.meta,
            })
            .collect())
    }

    async fn list_resource_templates(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_resource_templates(conn).await;
        }
        let raw = self
            .call_rpc(conn, "resources/templates/list", empty_source_object())
            .await?;
        let parsed: ResourceTemplatesListResult = decode_source(&raw)?;
        Ok(parsed
            .resource_templates
            .into_iter()
            .map(|t| McpResourceTemplateDto {
                uri_template: t.uri_template,
                name: t.name,
                description: t.description,
                mime_type: t.mime_type,
                annotations: t.annotations,
                meta: t.meta,
            })
            .collect())
    }

    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_prompts(conn).await;
        }
        let raw = self
            .call_rpc(conn, "prompts/list", empty_source_object())
            .await?;
        let parsed: PromptsListResult = decode_source(&raw)?;
        Ok(parsed
            .prompts
            .into_iter()
            .map(|p| McpPromptDto {
                name: p.name,
                description: p.description,
                arguments: p
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
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.call_tool(conn, tool, input).await;
        }
        let connection = self.connection_for_result(conn.connection_id)?;
        // Per-call budget resolved 1:1 with claude-code via the shared resolver
        // (the `MCP_TOOL_TIMEOUT` env var, else the ~27.8h default). Previously a
        // hardcoded 60s, which spuriously timed out legitimately long MCP tools.
        let timeout = mcp::client::mcp_tool_timeout();
        let params = self.decorate_params(
            conn.connection_id,
            "tools/call",
            source_object(vec![
                ("name".into(), source_string(tool)),
                ("arguments".into(), input),
            ]),
        )?;
        let raw = if self
            .negotiated_for(conn.connection_id)
            .is_some_and(|p| p.era == McpProtocolEra::Modern)
        {
            drive_modern_request(
                &JsonrpcMcpResultIo {
                    connection,
                    client_capabilities: modern_meta(
                        platform_common::mcp_remote::MODERN_PROTOCOL_VERSION,
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
                        secs: timeout.as_secs().max(1),
                    }
                }
                McpResultError::Connection(error) if is_method_not_found(&error) => {
                    McpError::ToolNotFound(tool.to_owned())
                }
                error => map_result_err(error),
            })?
        } else {
            let params = source_payload(&params)?;
            let result: JsonPayload = connection
                .call_with_timeout("tools/call", params, timeout)
                .await
                .map_err(|error| match &error {
                    ConnectionError::Router(RouterError::Timeout(_)) => McpError::Timeout {
                        server: String::new(),
                        tool: tool.to_owned(),
                        secs: timeout.as_secs().max(1),
                    },
                    _ if is_method_not_found(&error) => McpError::ToolNotFound(tool.to_owned()),
                    _ => map_call_err(&error),
                })?;
            source_from_payload(result)?
        };
        let parsed: ToolCallResult = decode_source(&raw)?;

        Ok(McpToolResultDto {
            content: parsed.content,
            is_error: parsed.is_error,
            meta: parsed.meta,
            structured_content: parsed.structured_content,
        })
    }

    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.read_resource(conn, uri).await;
        }
        let raw = self
            .call_rpc(
                conn,
                "resources/read",
                source_object(vec![("uri".into(), uri.clone())]),
            )
            .await?;
        let parsed: ResourceReadResult = decode_source(&raw)?;
        // Take the first contents block. `McpResourceContentDto.content` is a
        // single `String` whose documented contract is "text-encoded; binaries
        // are base64": map a UTF-8 `text` block through verbatim, and for a
        // binary `blob` block carry the base64 payload through UNDECODED (per
        // that contract) so the `lingxi-mcp` layer can base64-decode it when it
        // knows the resource is binary. Echo the request URI when the response
        // omits one.
        let first = parsed
            .contents
            .into_iter()
            .next()
            .ok_or_else(|| McpError::Internal("resources/read returned no contents".into()))?;
        let content = first
            .text
            .or_else(|| first.blob.map(Into::into))
            .unwrap_or_else(empty_source_string);
        Ok(McpResourceContentDto {
            uri: first.uri.unwrap_or_else(|| uri.clone()),
            content,
            mime_type: first.mime_type,
            meta: first.meta,
        })
    }

    async fn read_resource_rich(
        &self,
        conn: &McpRawConnection,
        uri: &Utf16JsonProjection,
        output_dir: &std::path::Path,
    ) -> Result<Vec<lingxi_core::host::McpResourceContentsRich>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.read_resource_rich(conn, uri, output_dir).await;
        }
        let raw = self
            .call_rpc(
                conn,
                "resources/read",
                source_object(vec![("uri".into(), uri.clone())]),
            )
            .await?;
        let parsed: ResourceReadResult = decode_source(&raw)?;
        // Map EVERY content block (not just the first) into the rich shape:
        // text → text, base64 blob → decode + persist under `output_dir`. The
        // logical server name is not tracked at this transport layer (the map is
        // keyed by connection id), so the `[Resource from <server> at <uri>] `
        // prefix uses an empty server name; the production read path that knows
        // the server name is `mcp::McpClient::read_resource_rich`.
        let contents: Vec<mcp::RawResourceContentRich> = parsed
            .contents
            .into_iter()
            .map(|c| mcp::RawResourceContentRich {
                uri: c.uri.unwrap_or_else(|| uri.clone()),
                mime_type: c.mime_type,
                meta: c.meta,
                text: c.text,
                blob: c.blob,
            })
            .collect();
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Ok(mcp::map_resource_contents(
            contents, "", output_dir, now_millis, "posix",
        ))
    }

    async fn ping(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        if self.is_remote_connection(conn_id) {
            return self.remote.ping(conn_id).await;
        }
        // Discard the result body (mock answers `{ "pong": true }`; the spec
        // answers `{}`). A failed ping is a connection-level failure.
        let conn = McpRawConnection {
            connection_id: conn_id,
        };
        let _: Utf16JsonProjection = self.call_rpc(&conn, "ping", empty_source_object()).await?;
        Ok(())
    }

    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.notifications(conn).await;
        }
        use futures::stream::unfold;
        let connection = self.connection_for_result(conn.connection_id)?;
        let rx = connection.notifications();
        // Adapt the `broadcast::Receiver<Notification>` into the trait's
        // `Stream<Item = McpNotificationDto>`. Lagged/closed receivers end the
        // stream; we drop lagged items rather than surfacing an error.
        let stream = unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(n) => {
                        return Some((
                            McpNotificationDto {
                                method: n.method,
                                params: n
                                    .params
                                    .as_ref()
                                    .and_then(source_from_payload_ref)
                                    .unwrap_or_else(|| {
                                        Utf16JsonProjection::parse("null")
                                            .expect("null JSON is valid")
                                    }),
                            },
                            rx,
                        ));
                    }
                    // Lagged: skip dropped notifications, keep listening.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    // Closed: the sender (broker) is gone — end the stream.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }

    async fn handle_elicitation(
        &self,
        conn: &McpRawConnection,
        req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.handle_elicitation(conn, req).await;
        }
        Err(McpError::Internal(
            "posix mcp elicitation delegated to lingxi-mcp::McpClient (M2-02b)".into(),
        ))
    }

    fn disconnect_sync(&self, conn_id: McpConnectionId) {
        if self.is_remote_connection(conn_id) {
            self.remote.disconnect_sync(conn_id);
            return;
        }
        if let Ok(mut guard) = self.negotiated.lock() {
            guard.remove(&conn_id);
        }
        if let Ok(mut guard) = self.metadata.lock() {
            guard.remove(&conn_id);
        }
        if let Ok(mut guard) = self.elicitation.lock() {
            guard.remove(&conn_id);
        }
        let entry = self
            .connections
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&conn_id));
        match entry {
            Some(PosixMcpConnection::Stdio {
                connection,
                reaper_kill,
                ..
            }) => {
                // Actively kill the child rather than rely on stdin-EOF:
                // signal the reaper task to `child.start_kill()` so a server
                // that ignores stdin EOF is still force-terminated.
                // `kill_on_drop(true)` does NOT save us here because the
                // detached reaper owns the `Child`.
                if let Ok(mut guard) = reaper_kill.lock() {
                    if let Some(tx) = guard.take() {
                        // The receiver is only dropped if the reaper already
                        // observed child exit; a send error just means the
                        // child is already gone, which is the desired end
                        // state.
                        let _ = tx.send(());
                    }
                }
                // Abort the broker reader/writer tasks (outbound calls now
                // fail with WriterClosed). Dropping the sink also closes the
                // child's stdin, but the explicit kill above is what
                // guarantees teardown.
                connection.close();
            }
            None => {}
        }
    }

    async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        self.disconnect_sync(conn_id);
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![
            McpTransportKind::Stdio,
            McpTransportKind::Sse,
            McpTransportKind::Http,
            McpTransportKind::SseIde,
            McpTransportKind::WsIde,
        ]
    }
}

fn map_kind(spec: &McpTransportSpec) -> McpTransportKind {
    match spec {
        McpTransportSpec::Stdio { .. } => McpTransportKind::Stdio,
        McpTransportSpec::Sse { .. } => McpTransportKind::Sse,
        McpTransportSpec::Http { .. } => McpTransportKind::Http,
        McpTransportSpec::WebSocket { .. } => McpTransportKind::WebSocket,
        McpTransportSpec::InProcess { .. } => McpTransportKind::InProcess,
        McpTransportSpec::SseIde { .. } => McpTransportKind::SseIde,
        McpTransportSpec::WsIde { .. } => McpTransportKind::WsIde,
        McpTransportSpec::SdkControl { .. } => McpTransportKind::SdkControl,
    }
}

/// Error type returned by `spawn_stdio` (and, in M2-02c Task 5, `connect_ws`).
#[derive(Debug, thiserror::Error)]
pub enum McpTransportError {
    /// Failed to spawn the child process.
    #[error("io: {0}")]
    Io(String),
    /// Failed to acquire one of the stdin/stdout/stderr pipes from the child.
    #[error("missing stdio pipe: {0}")]
    MissingPipe(&'static str),
}

/// Spawn an MCP child over stdio and return a fully-wired
/// `jsonrpc::Connection`.
///
/// - Frames stdin/stdout with `LineCodec` (NDJSON: one JSON object per
///   `\n`-terminated line).
/// - Drains stderr into a 64 MB `StderrRing` (drop-oldest on overflow). The
///   buffer is held behind the returned [`StdioHandles::stderr`] handle so
///   the caller can snapshot stderr if the child crashes during initialize.
/// - Sets `kill_on_drop(true)` on the child as a backstop for full runtime
///   shutdown, and spawns a reaper task that owns the `Child`. The reaper
///   waits on either child exit OR a one-shot kill signal
///   ([`StdioHandles::reaper_kill`]); on the kill signal it calls
///   `child.start_kill()` and awaits exit, so a non-cooperative server that
///   ignores stdin-EOF is still force-terminated on disconnect.
/// - Propagates child exit by closing the connection's broker (via the
///   spawned waiter task on stdout EOF, which the broker observes
///   naturally).
///
/// # Errors
///
/// - [`McpTransportError::Io`] if the child fails to spawn (e.g. command not
///   found, permission denied, cwd does not exist).
/// - [`McpTransportError::MissingPipe`] if `Stdio::piped()` failed to attach
///   one of the three pipes — should not happen in practice but is reported
///   rather than panicked on.
pub async fn spawn_stdio(cfg: StdioConfig) -> Result<Connection, McpTransportError> {
    let StdioHandles { connection, .. } = spawn_stdio_with_handles(cfg).await?;
    Ok(connection)
}

/// Handles returned by [`spawn_stdio_with_handles`] — the same `Connection`
/// that [`spawn_stdio`] returns, plus a shared handle on the stderr ring
/// buffer so callers can snapshot any buffered stderr if the child misbehaves,
/// plus a one-shot kill signal that force-terminates the child.
#[non_exhaustive]
pub struct StdioHandles {
    /// The fully-wired JSON-RPC `Connection` over the child's stdio.
    pub connection: Connection,
    /// Shared `StderrRing` populated by a background drain task.
    pub stderr: Arc<AsyncMutex<StderrRing>>,
    /// Send `()` to make the reaper task call `child.start_kill()`, then await
    /// its exit. This is the only reliable way to force-kill a child that
    /// ignores stdin-EOF: the reaper owns the `Child`, so `kill_on_drop(true)`
    /// alone never fires until full runtime shutdown.
    pub reaper_kill: tokio::sync::oneshot::Sender<()>,
    /// Becomes true after the reaper has observed and reaped the child exit.
    pub reaper_done: tokio::sync::watch::Receiver<bool>,
}

// Re-export the shared WebSocket connector so callers can use a single path
// (`platform_posix::mcp::connect_ws`) without reaching into the
// `lingxi_platform_common` crate directly.
pub use platform_common::mcp_ws::{connect_ws, WsConnectError, AUTH_HEADER_NAME, WS_SUBPROTOCOL};

/// Same as [`spawn_stdio`] but also surfaces the shared stderr ring buffer
/// so callers can inspect captured stderr after the child exits or hangs.
///
/// The function is `async` to leave room for a future initialize handshake
/// without breaking callers — today every `.await` happens inside the
/// spawned background tasks, so clippy's `unused_async` is allowed here.
#[allow(clippy::unused_async)]
pub async fn spawn_stdio_with_handles(cfg: StdioConfig) -> Result<StdioHandles, McpTransportError> {
    let mut cmd = tokio::process::Command::new(&cfg.cmd);
    cmd.args(&cfg.args);
    // The child inherits the parent's environment, then overrides with
    // `cfg.env`. Callers are responsible for filtering secrets out of
    // `cfg.env` before constructing the config.
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `kill_on_drop` ensures the child dies if the wait task is dropped
    // (e.g. on `Connection` drop, since the wait task owns the `Child`).
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| McpTransportError::Io(e.to_string()))?;

    let stdin = child
        .stdin
        .take()
        .ok_or(McpTransportError::MissingPipe("stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(McpTransportError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(McpTransportError::MissingPipe("stderr"))?;

    // Drain stderr into a shared `StderrRing` on a background task.
    let stderr_ring = Arc::new(AsyncMutex::new(StderrRing::new(StderrRing::DEFAULT_CAP)));
    {
        let ring = stderr_ring.clone();
        tokio::spawn(async move {
            let mut reader = stderr;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut guard = ring.lock().await;
                        guard.push(&buf[..n]);
                    }
                }
            }
        });
    }

    // Wire the JSON-RPC connection over NDJSON stdio.
    let connection = Connection::new_line_delimited(stdout, stdin);

    // Reap the child on exit. The waiter task owns the `Child`, so
    // `kill_on_drop(true)` makes the child die if this task is dropped
    // (e.g. on runtime shutdown). When the child exits normally, its
    // stdout closes and the broker shuts down without further action.
    //
    // A `disconnect`/teardown path sends on `reaper_kill`, which makes the
    // reaper call `child.start_kill()` and await exit — this force-kills a
    // server that would otherwise ignore stdin-EOF and run forever.
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    let (reaper_done_tx, reaper_done) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::select! {
            wait = child.wait() => match wait {
                Ok(status) => tracing::debug!(?status, "mcp stdio child exited"),
                Err(e) => tracing::warn!(error = %e, "mcp stdio child wait failed"),
            },
            recv = kill_rx => match recv {
                // Explicit kill requested via `disconnect`: force-terminate the
                // child and reap it so it does not linger as a zombie.
                Ok(()) => {
                    if let Err(e) = child.start_kill() {
                        tracing::warn!(error = %e, "mcp stdio child start_kill failed");
                    }
                    match child.wait().await {
                        Ok(status) => tracing::debug!(?status, "mcp stdio child killed"),
                        Err(e) => {
                            tracing::warn!(error = %e, "mcp stdio child wait-after-kill failed");
                        }
                    }
                }
                // Sender dropped WITHOUT a kill request (e.g. a `spawn_stdio`
                // caller that does not retain the handle). Do NOT kill — fall
                // back to reaping the child on its own exit, preserving the
                // pre-kill-channel behavior. `kill_on_drop(true)` still backs
                // us up on full runtime shutdown.
                Err(_) => match child.wait().await {
                    Ok(status) => tracing::debug!(?status, "mcp stdio child exited"),
                    Err(e) => tracing::warn!(error = %e, "mcp stdio child wait failed"),
                },
            }
        }
        let _ = reaper_done_tx.send(true);
    });

    Ok(StdioHandles {
        connection,
        stderr: stderr_ring,
        reaper_kill: kill_tx,
        reaper_done,
    })
}

#[cfg(test)]
mod re_export_tests {
    /// Verify the posix crate exposes the public `connect_ws` symbol at
    /// `platform_posix::mcp::connect_ws` (callers should not have to
    /// import from `lingxi_platform_common` directly).
    #[allow(unused_imports)]
    use crate::mcp::connect_ws;
}

#[cfg(test)]
mod initialize_params_tests {
    //! MCP `initialize` request payload parity with claude-code
    //! `services/mcp/client.ts:985-1002`.
    use super::{initialize_params, CLIENT_DESCRIPTION, MCP_PROTOCOL_VERSION};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    use serde_json::json;
    // Imported here, not at file scope: the lib target has no other user, so a
    // file-level `use` reads as an unused import and gets deleted — which then
    // breaks these tests, since they reached it through `super::`.
    use platform_common::mcp_remote::directory_read_capability;

    fn source(value: &serde_json::Value) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(&value.to_string()).expect("fixture is valid JSON")
    }

    #[test]
    fn capabilities_advertise_roots_and_current_elicitation_modes() {
        let params = initialize_params();
        let caps = params.subprojection("/capabilities").unwrap();
        let caps_obj = caps.object_entries().expect("capabilities object");
        // EXACTLY the two markers claude-code advertises — nothing else.
        assert_eq!(
            caps_obj.len(),
            2,
            "capabilities must have exactly 2 keys (roots, elicitation), got {:?}",
            caps_obj
                .iter()
                .map(|entry| entry.display_key)
                .collect::<Vec<_>>(),
        );
        let roots = caps.subprojection("/roots").unwrap();
        assert_eq!(roots.to_json_string().unwrap(), r#"{"listChanged":true}"#);
        let elicitation = caps.subprojection("/elicitation").unwrap();
        assert_eq!(
            elicitation.to_json_string().unwrap(),
            r#"{"form":{},"url":{}}"#,
            "current default includes both form and URL elicitation",
        );
    }

    #[test]
    fn client_info_carries_claude_code_identity_literals() {
        let params = initialize_params();
        let info = params.subprojection("/clientInfo").unwrap();
        assert_eq!(info.string_at("/name"), Some("lingxi"));
        assert_eq!(info.string_at("/title"), Some("LingXi"));
        assert_eq!(
            info.string_at("/description"),
            Some("An agentic coding tool")
        );
        assert_eq!(
            info.string_at("/websiteUrl"),
            Some("https://claude.com/claude-code")
        );
        // The literals are sourced from the canonical `mcp::identity` constants
        // (so a rename there propagates here) — cross-check the reused values.
        assert_eq!(info.string_at("/name"), Some(mcp::CLIENT_NAME));
        assert_eq!(info.string_at("/title"), Some(mcp::CLIENT_TITLE));
        assert_eq!(info.string_at("/websiteUrl"), Some(mcp::MCP_WEBSITE_URL));
        assert_eq!(info.string_at("/description"), Some(CLIENT_DESCRIPTION));
        // `version` stays THIS build's product version — never faked to look
        // like claude-code's release — and must look semver-like.
        let version = info.string_at("/version").expect("version present");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert!(
            version.split('.').count() >= 3,
            "version must look semver-like, got {version:?}",
        );
    }

    #[test]
    fn wire_bytes_are_camelcase_and_carry_literal_markers() {
        let s = initialize_params().to_json_string().expect("serialize");
        assert_eq!(
            initialize_params().string_at("/protocolVersion"),
            Some(MCP_PROTOCOL_VERSION)
        );
        assert!(
            s.contains(r#""name":"lingxi""#),
            "wire bytes must carry literal lingxi name, got: {s}",
        );
        assert!(
            s.contains(r#""websiteUrl":"https://claude.com/claude-code""#),
            "websiteUrl must be camelCase, got: {s}",
        );
        // No stale `claude-code` client name and no snake_case `website_url` leak.
        assert!(
            !s.contains(r#""name":"claude-code""#),
            "stale claude-code name leaked"
        );
        assert!(!s.contains("website_url"), "snake_case website_url leaked");
    }

    #[test]
    fn directory_read_capability_decodes_none_absent_false_and_true() {
        assert!(!directory_read_capability(None));
        let empty = source(&serde_json::json!({}));
        let false_cap = source(&serde_json::json!({
            "extensions": {
                "io.modelcontextprotocol/skills": {
                    "directoryRead": false
                }
            }
        }));
        let true_cap = source(&serde_json::json!({
            "extensions": {
                "io.modelcontextprotocol/skills": {
                    "directoryRead": true
                }
            }
        }));
        assert!(!directory_read_capability(Some(&empty)));
        assert!(!directory_read_capability(Some(&false_cap)));
        assert!(directory_read_capability(Some(&true_cap)));
    }
}

#[cfg(test)]
mod error_mapping_tests {
    use super::{handshake_error, is_method_not_found, map_call_err};
    use jsonrpc::{ConnectionError, JsonPayload, JsonRpcError, RouterError};
    use lingxi_core::host::McpError;
    use serde_json::json;
    use std::time::Duration;

    /// Example seconds value for the Display-format assertion below. The
    /// production timeout is resolved at call time via
    /// `mcp::client::mcp_tool_timeout`, so this is a fixed illustrative value.
    const EXAMPLE_TIMEOUT_SECS: u64 = 60;

    /// A remote `-32601` is recognized as a method-not-found error (the MCP
    /// convention for an unknown tool); other remote codes are not.
    #[test]
    fn is_method_not_found_matches_only_minus_32601() {
        let mnf = ConnectionError::Router(RouterError::Remote(JsonRpcError {
            code: jsonrpc::METHOD_NOT_FOUND,
            message: "unknown tool: nope".into(),
            data: None,
        }));
        assert!(is_method_not_found(&mnf));

        let other = ConnectionError::Router(RouterError::Remote(JsonRpcError {
            code: -32000,
            message: "server error".into(),
            data: None,
        }));
        assert!(!is_method_not_found(&other));

        // A non-remote error (e.g. timeout) is never method-not-found.
        assert!(!is_method_not_found(&ConnectionError::Router(
            RouterError::Timeout(Duration::from_secs(1))
        )));
    }

    /// `map_call_err` funnels into the generic `McpError::Internal` surface,
    /// preserving the underlying Display string.
    #[test]
    fn map_call_err_is_internal() {
        let e = ConnectionError::Router(RouterError::WriterClosed);
        match map_call_err(&e) {
            McpError::Internal(s) => {
                let expected = e.to_string();
                assert_eq!(s.string_at(""), Some(expected.as_str()));
            }
            other => panic!("expected McpError::Internal, got {other:?}"),
        }
    }

    /// `handshake_error` unwraps the synthetic `{httpStatus, wwwAuthenticate}`
    /// `data` shape the Streamable HTTP writer task attaches to a non-2xx
    /// `initialize` POST (`mcp_http.rs::http_error_message`) into a
    /// structural `McpError::HttpResponse`, carrying BOTH the exact status
    /// and the full `WWW-Authenticate` value through — the pair §19's
    /// AUTH_HEADER_REJECTED/HEADERS_HELPER_AUTH_REJECTED classification and
    /// §24c's `resource_metadata` extraction both need.
    #[test]
    fn handshake_error_unwraps_structured_http_data() {
        let e = ConnectionError::Router(RouterError::Remote(JsonRpcError {
            code: -32001,
            message: "MCP_HTTP_STATUS=403;WWW_AUTHENTICATE=Bearer error=\"insufficient_scope\", \
                      scope=\"mcp:elevated\", resource_metadata=\"https://mock/.well-known/x\""
                .into(),
            data: Some(JsonPayload::from_serializable(&json!({
                "httpStatus": 403,
                "wwwAuthenticate": "Bearer error=\"insufficient_scope\", scope=\"mcp:elevated\", \
                                     resource_metadata=\"https://mock/.well-known/x\""
            })).unwrap()),
        }));
        match handshake_error(&e) {
            McpError::HttpResponse {
                status,
                www_authenticate,
            } => {
                assert_eq!(status, 403);
                let waa = www_authenticate.expect("wwwAuthenticate must survive unwrapping");
                assert!(waa.contains("insufficient_scope"));
                assert!(waa.contains("resource_metadata="));
            }
            other => panic!("expected McpError::HttpResponse, got {other:?}"),
        }
    }

    /// A 401 with no `WWW-Authenticate` header still structures as
    /// `HttpResponse { status: 401, www_authenticate: None }` — the header is
    /// optional, the status is not.
    #[test]
    fn handshake_error_unwraps_structured_http_data_without_www_authenticate() {
        let e = ConnectionError::Router(RouterError::Remote(JsonRpcError {
            code: -32001,
            message: "MCP_HTTP_STATUS=401;WWW_AUTHENTICATE=".into(),
            data: Some(
                JsonPayload::from_serializable(
                    &json!({ "httpStatus": 401, "wwwAuthenticate": null }),
                )
                .unwrap(),
            ),
        }));
        assert!(matches!(
            handshake_error(&e),
            McpError::HttpResponse {
                status: 401,
                www_authenticate: None,
            }
        ));
    }

    /// A genuine MCP protocol failure (unrelated remote error, no `data`)
    /// must NOT be misclassified as an `HttpResponse` — it falls back to the
    /// prior stringified `Handshake` behavior unchanged.
    #[test]
    fn handshake_error_falls_back_to_handshake_for_non_http_errors() {
        let remote = ConnectionError::Router(RouterError::Remote(JsonRpcError {
            code: -32602,
            message: "invalid params".into(),
            data: None,
        }));
        match handshake_error(&remote) {
            McpError::Handshake(s) => {
                let expected = remote.to_string();
                assert_eq!(s.string_at(""), Some(expected.as_str()));
            }
            other => panic!("expected McpError::Handshake, got {other:?}"),
        }

        let writer_closed = ConnectionError::Router(RouterError::WriterClosed);
        match handshake_error(&writer_closed) {
            McpError::Handshake(s) => {
                let expected = writer_closed.to_string();
                assert_eq!(s.string_at(""), Some(expected.as_str()));
            }
            other => panic!("expected McpError::Handshake, got {other:?}"),
        }
    }

    /// The load-bearing `McpError::Timeout` Display string (matched by REPL /
    /// integration surfaces) must carry the server, tool, and seconds in the
    /// documented `traits` format.
    #[test]
    fn timeout_display_string_is_load_bearing() {
        let err = McpError::Timeout {
            server: String::new(),
            tool: "echo".into(),
            secs: EXAMPLE_TIMEOUT_SECS,
        };
        assert_eq!(
            err.to_string(),
            format!("MCP server \"\" tool \"echo\" timed out after {EXAMPLE_TIMEOUT_SECS}s")
        );
    }
}

#[cfg(test)]
mod tool_meta_tests {
    use super::{source_from_test_json, RawResourceTemplate, RawTool, ToolsListResult};

    /// §27b — the `_meta.anthropic/requiresUserInteraction` bit must survive
    /// THIS transport, because `McpRegistry::connect` fills
    /// `McpConnectionState::Connected { tools }` from
    /// `PosixMcpTransport::list_tools`, and `build_registered_mcp_tools` reads
    /// that state to construct every desktop `MCPTool`. The parallel decode in
    /// `mcp::client::McpClient::list_tools` is reached only by
    /// `refresh_catalog`, so a test there does NOT cover the connect path:
    /// with the bit hardcoded `false` here a server declaring
    /// `requiresUserInteraction` still got "Yes, allow always" offered.
    #[test]
    fn requires_user_interaction_meta_survives_the_posix_transport_decode() {
        let raw = serde_json::json!({
            "tools": [
                {
                    "name": "plain",
                    "description": "no meta",
                    "inputSchema": { "type": "object" }
                },
                {
                    "name": "interactive",
                    "description": "needs a live consent step",
                    "inputSchema": { "type": "object" },
                    "outputSchema": { "type": "object", "properties": { "ok": { "type": "boolean" } } },
                    "annotations": { "title": "Interactive", "openWorldHint": true,
                        "vendor/riskTier": "reviewed" },
                    "icons": [{ "src": "https://example.invalid/icon.svg",
                        "vendor/accent": "blue" }],
                    "_meta": {
                        "anthropic/searchHint": "interactive consent",
                        "anthropic/alwaysLoad": true,
                        "anthropic/requiresUserInteraction": true,
                        "openai/outputTemplate": "ui://example/widget.html"
                    }
                }
            ]
        });
        let raw_json = raw.to_string();
        let parsed: ToolsListResult = serde_json::from_str(&raw_json).expect("decode");
        let dtos: Vec<_> = parsed.tools.into_iter().map(RawTool::into_dto).collect();
        assert_eq!(dtos.len(), 2);
        assert_eq!(dtos[0].tool_name, "plain");
        assert!(
            !dtos[0].requires_user_interaction,
            "a tool with no _meta must default to false"
        );
        assert_eq!(dtos[1].tool_name, "interactive");
        assert_eq!(
            dtos[1]
                .search_hint
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("interactive consent")
        );
        assert_eq!(dtos[1].always_load, Some(true));
        assert!(
            dtos[1].requires_user_interaction,
            "_meta.anthropic/requiresUserInteraction must reach the DTO the registry stores"
        );
        assert!(dtos[1]
            .output_schema
            .as_ref()
            .unwrap()
            .semantically_eq(&source_from_test_json(&serde_json::json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } }
            }))));
        assert_eq!(
            dtos[1]
                .annotations
                .as_ref()
                .and_then(|a| a.title.as_ref())
                .and_then(|title| title.string_at("")),
            Some("Interactive")
        );
        assert_eq!(dtos[1].icons.len(), 1);
        assert_eq!(
            dtos[1]
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.extra.string_at("/vendor~1riskTier")),
            Some("reviewed")
        );
        assert_eq!(
            dtos[1].icons[0].extra.string_at("/vendor~1accent"),
            Some("blue")
        );
        assert!(dtos[1]
            .meta
            .as_ref()
            .unwrap()
            .semantically_eq(&source_from_test_json(&serde_json::json!({
                "anthropic/searchHint": "interactive consent",
                "anthropic/alwaysLoad": true,
                "anthropic/requiresUserInteraction": true,
                "openai/outputTemplate": "ui://example/widget.html"
            }))));
    }

    #[test]
    fn resource_template_metadata_survives_the_posix_transport_decode() {
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
}

#[cfg(test)]
mod protocol_era_tests {
    use super::*;

    #[test]
    fn modern_probe_carries_the_required_meta_keys() {
        let value = modern_probe_params(lingxi_core::host::McpElicitationMode::FormAndUrl);
        let meta = value.subprojection("/_meta").unwrap();
        let meta_fields = meta.object_entries().unwrap();

        assert_eq!(
            meta_fields.len(),
            3,
            "modern probe _meta must be exactly three keys"
        );
        assert!(meta
            .get_property("io.modelcontextprotocol/protocolVersion")
            .is_some());
        assert!(meta
            .get_property("io.modelcontextprotocol/clientInfo")
            .is_some());
        assert!(meta
            .get_property("io.modelcontextprotocol/clientCapabilities")
            .is_some());
        assert_eq!(
            meta.string_at("/io.modelcontextprotocol~1protocolVersion"),
            Some(platform_common::mcp_remote::MODERN_PROTOCOL_VERSION)
        );
        assert_eq!(value.object_entries().unwrap().len(), 1);
    }

    #[test]
    fn modern_result_envelope_is_strict_and_strips_complete_marker() {
        let complete = source_from_test_json(
            &json!({"resultType": "complete", "ttlMs":0,"cacheScope":"private","tools": []}),
        );

        let DecodedMcpResult::Complete(complete) =
            decode_modern_result("tools/list", complete).expect("complete envelope")
        else {
            panic!("complete expected")
        };
        assert!(complete.get_property("resultType").is_none());

        for invalid in [
            json!({"tools": []}),
            json!({"resultType": "partial"}),
            json!(null),
        ] {
            assert!(decode_modern_result("tools/list", source_from_test_json(&invalid)).is_err());
        }
    }
}
