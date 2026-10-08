//! `MockMcpTransport` — minimal in-memory MCP transport for engine tests.
//!
//! Tests pre-register tool DTOs with [`MockMcpTransport::add_tool`] and
//! then drive `mcp::McpRegistry::connect` against the mock. The
//! mock is intentionally tiny: it only implements enough of
//! [`McpTransport`] to walk the connect → initialize → `list_tools` path.

#![allow(clippy::unwrap_used)] // Mutex lock failures here mean the test is broken.

use async_trait::async_trait;
use bytes::Bytes;
use jsonrpc::{Connection, Mode};
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
    McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-server `resources/list` behavior the responder applies (Batch 5c).
/// Keyed by the server's `InProcess` `registry_key` (== the config name).
#[derive(Clone)]
enum ResourceBehavior {
    /// Answer `resources/list` with these `(uri, name)` rows.
    List(Vec<(String, String)>),
    /// Answer `resources/list` with a JSON-RPC error (server failure) so the
    /// all-servers `ListMcpResources` path can prove error-isolation.
    Error,
}

/// In-memory MCP transport that returns canned responses to the registry.
pub struct MockMcpTransport {
    tools: Mutex<Vec<McpToolDto>>,
    /// Paired in-memory `jsonrpc::Connection`s minted per `connect`, keyed by
    /// the `McpConnectionId` handed back. Exposed via [`RawConnectionProvider`]
    /// so `McpRegistry::with_raw_conn` can bridge a live `McpClient`.
    conns: Mutex<HashMap<McpConnectionId, Arc<Connection>>>,
    /// When `true`, each minted paired connection gets a background responder
    /// task that answers the client's outbound `tools/call` requests with a
    /// canned `{content: "ok", isError: false}` result, so a live `McpClient`
    /// bridged through `RawConnectionProvider` actually round-trips (the
    /// default presence-only mode drops the peer ends and never responds).
    /// Opt-in via [`MockMcpTransport::with_call_responder`] so the existing
    /// presence-only lifecycle tests are unaffected.
    respond_to_calls: bool,
    /// Records each FQN passed to a responder's `tools/call` so a test can
    /// assert dispatch actually reached the wire. Only populated when
    /// `respond_to_calls` is set.
    called_tools: Arc<Mutex<Vec<String>>>,
    /// Per-server `resources/list` behavior, keyed by `InProcess` `registry_key`
    /// (== config name). Consulted by the responder so a multi-server
    /// all-servers `ListMcpResources` test can give each server its own
    /// resources (or an error). Only used with `respond_to_calls`.
    resources: Mutex<HashMap<String, ResourceBehavior>>,
    /// Records the `method` of every id-less inbound notification frame the
    /// client emits (e.g. `notifications/roots/list_changed`), so a test can
    /// assert the roots-changed fan-out reached the server. Only populated when
    /// `respond_to_calls` is set (the responder task reads the client's stream).
    observed_notifications: Arc<Mutex<Vec<String>>>,
}

impl Default for MockMcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockMcpTransport {
    /// Build a fresh mock with no tools (presence-only: minted paired
    /// connections have NO responder, matching the lifecycle tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: Mutex::new(Vec::new()),
            conns: Mutex::new(HashMap::new()),
            respond_to_calls: false,
            called_tools: Arc::new(Mutex::new(Vec::new())),
            resources: Mutex::new(HashMap::new()),
            observed_notifications: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build a mock whose minted paired connections spawn a background
    /// responder that answers the bridged `McpClient`'s outbound `tools/call`
    /// requests, so dispatch through the live client round-trips (Batch 3
    /// invocation test). Additive — `new()` behavior is unchanged.
    #[must_use]
    pub fn with_call_responder() -> Self {
        Self {
            respond_to_calls: true,
            ..Self::new()
        }
    }

    /// The FQNs the responder observed on `tools/call` (in call order). Empty
    /// unless built via [`MockMcpTransport::with_call_responder`].
    #[must_use]
    pub fn called_tools(&self) -> Vec<String> {
        self.called_tools.lock().unwrap().clone()
    }

    /// The `method`s of every inbound notification the client emitted (in
    /// order), e.g. `notifications/roots/list_changed` from the roots-changed
    /// fan-out. Empty unless built via [`MockMcpTransport::with_call_responder`].
    #[must_use]
    pub fn observed_notifications(&self) -> Vec<String> {
        self.observed_notifications.lock().unwrap().clone()
    }

    /// Make the server identified by `registry_key` answer `resources/list`
    /// with the given `(uri, name)` rows (Batch 5c). Each subsequent `connect`
    /// for that `InProcess { registry_key }` spec wires its responder to return
    /// them. Requires [`MockMcpTransport::with_call_responder`].
    pub fn set_resources(&self, registry_key: &str, rows: &[(&str, &str)]) {
        let list = rows
            .iter()
            .map(|(u, n)| ((*u).to_string(), (*n).to_string()))
            .collect();
        self.resources
            .lock()
            .unwrap()
            .insert(registry_key.to_string(), ResourceBehavior::List(list));
    }

    /// Make the server identified by `registry_key` answer `resources/list`
    /// with a JSON-RPC error (Batch 5c error-isolation test). Requires
    /// [`MockMcpTransport::with_call_responder`].
    pub fn set_resources_error(&self, registry_key: &str) {
        self.resources
            .lock()
            .unwrap()
            .insert(registry_key.to_string(), ResourceBehavior::Error);
    }

    /// Register a tool named `name` under the server label `mock`.
    ///
    /// The full name follows the engine's convention `mcp__<server>__<tool>`.
    pub fn add_tool(&self, name: &str) {
        self.tools.lock().unwrap().push(McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: "mock".into(),
            tool_name: name.into(),
            description: format!("{name} test tool"),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: format!("mcp__mock__{name}"),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        });
    }
}

/// Build a `Connection` over a fresh pair of `mpsc<Bytes>` channels (the
/// `paired_connection` pattern from `mcp/src/client.rs:536`). The peer ends are
/// dropped — lifecycle tests assert client *presence*, not wire round-trips.
fn paired_connection() -> Arc<Connection> {
    let (_peer_to_us_tx, peer_to_us_rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    let (us_to_peer_tx, _us_to_peer_rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    Arc::new(Connection::new_streams(
        peer_to_us_rx,
        us_to_peer_tx,
        Mode::Lines,
    ))
}

/// Build a paired `Connection` AND spawn a background responder that answers
/// the client's outbound `tools/call` requests (line-framed JSON-RPC) with a
/// canned `{content: "ok", isError: false}` result, echoing the request `id`.
/// Each observed FQN is recorded into `called_tools`. Used by the Batch 3
/// invocation test so a live bridged `McpClient` round-trips.
///
/// `resources` (Batch 5c): how this connection answers `resources/list` —
/// `Some(List)` returns the configured rows, `Some(Error)` returns a JSON-RPC
/// error, `None` returns an empty list.
fn responding_connection(
    called_tools: Arc<Mutex<Vec<String>>>,
    resources: Option<ResourceBehavior>,
    observed_notifications: Arc<Mutex<Vec<String>>>,
) -> Arc<Connection> {
    // `peer_to_us`: peer (responder) → client (responses).
    // `us_to_peer`: client → peer (the outbound requests we answer).
    let (peer_to_us_tx, peer_to_us_rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    let (us_to_peer_tx, mut us_to_peer_rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    let conn = Arc::new(Connection::new_streams(
        peer_to_us_rx,
        us_to_peer_tx,
        Mode::Lines,
    ));
    tokio::spawn(async move {
        while let Some(frame) = us_to_peer_rx.recv().await {
            let Ok(req) = serde_json::from_slice::<Value>(&frame) else {
                continue;
            };
            // Notifications (e.g. `notifications/initialized`,
            // `notifications/roots/list_changed`) carry no `id`. Record the
            // method so a test can assert the roots-changed fan-out arrived,
            // then ignore them — only id-bearing requests get a response.
            let Some(id) = req.get("id").cloned() else {
                if let Some(method) = req.get("method").and_then(Value::as_str) {
                    observed_notifications
                        .lock()
                        .unwrap()
                        .push(method.to_string());
                }
                continue;
            };
            let method = req.get("method").and_then(Value::as_str);
            // Record the dispatched tool name from the `tools/call` params.
            if method == Some("tools/call") {
                if let Some(name) = req
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                {
                    called_tools.lock().unwrap().push(name.to_string());
                }
            }
            // `resources/list` (Batch 5c): answer per the configured behavior.
            let resp = if method == Some("resources/list") {
                match &resources {
                    Some(ResourceBehavior::Error) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32000, "message": "mock resources/list failure" },
                    }),
                    Some(ResourceBehavior::List(rows)) => {
                        let arr: Vec<Value> = rows
                            .iter()
                            .map(|(uri, name)| serde_json::json!({ "uri": uri, "name": name }))
                            .collect();
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "resources": arr },
                        })
                    }
                    None => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "resources": [] },
                    }),
                }
            } else {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": "ok", "isError": false },
                })
            };
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n'); // LineCodec frames on newline.
            if peer_to_us_tx.send(Bytes::from(bytes)).await.is_err() {
                break; // client connection dropped.
            }
        }
    });
    conn
}

#[async_trait]
impl McpTransport for MockMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let connection_id = McpConnectionId::new();
        // Stash a paired connection so `RawConnectionProvider::connection_for`
        // can hand the registry a live `Arc<jsonrpc::Connection>`. With the
        // responder opt-in, the peer side answers the client's `tools/call`.
        let conn = if self.respond_to_calls {
            // Look up this server's `resources/list` behavior (Batch 5c) by its
            // `InProcess` registry_key (== config name).
            let resources = match spec {
                McpTransportSpec::InProcess { registry_key } => {
                    self.resources.lock().unwrap().get(registry_key).cloned()
                }
                _ => None,
            };
            responding_connection(
                self.called_tools.clone(),
                resources,
                self.observed_notifications.clone(),
            )
        } else {
            paired_connection()
        };
        self.conns.lock().unwrap().insert(connection_id, conn);
        Ok(McpRawConnection { connection_id })
    }

    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: true,
            resources: true,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: std::collections::HashMap::new(),
            extensions: std::collections::HashMap::new(),
        })
    }

    async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        Ok(self.tools.lock().unwrap().clone())
    }

    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_prompts(&self, _conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        _tool: &str,
        _input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        Ok(McpToolResultDto {
            result_projection: None,

            content: serde_json::json!("ok"),
            is_error: false,
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        Err(McpError::Internal("not implemented".into()))
    }

    async fn ping(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        use futures::stream::empty;
        Ok(Box::pin(empty()))
    }

    async fn handle_elicitation(
        &self,
        _c: &McpRawConnection,
        _r: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Err(McpError::Internal("not implemented".into()))
    }

    async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        self.conns.lock().unwrap().remove(&conn_id);
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio, McpTransportKind::InProcess]
    }
}

impl mcp::RawConnectionProvider for MockMcpTransport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<Connection>> {
        self.conns.lock().unwrap().get(&id).cloned()
    }
}
