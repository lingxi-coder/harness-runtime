//! Bridge (Batch 1) + FQN-rewrite / normalize-match (Batch 2) tests.
//!
//! A broader mock-transport lifecycle test lives in
//! `test-harness/tests/mcp_lifecycle.rs`.
use super::*;
use crate::connection::{ConfigScope, McpServerConfig};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use jsonrpc::{Connection, Mode};
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
    McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId as ConnId;
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex as TestMutex;
use std::task::{Context, Poll, Wake, Waker};
use tokio::sync::{mpsc, Notify};

/// Build a `Connection` over a fresh pair of `mpsc<Bytes>` channels (the
/// `paired_connection` pattern from `client.rs:536`); the peer ends are
/// dropped — these tests only assert client *presence*, not round-trips.
fn paired_connection() -> Arc<Connection> {
    let (_peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
    let (us_to_peer_tx, _us_to_peer_rx) = mpsc::channel::<Bytes>(8);
    Arc::new(Connection::new_streams(
        peer_to_us_rx,
        us_to_peer_tx,
        Mode::Lines,
    ))
}

/// Paired connection retaining the mock-server ends for notification and
/// request/response catalog-refresh tests.
#[allow(clippy::type_complexity)]
fn drivable_connection() -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
    let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
    let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
    (
        Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        )),
        peer_to_us_tx,
        us_to_peer_rx,
    )
}

/// Functional mock transport that ALSO bridges a paired in-memory
/// `jsonrpc::Connection` through [`RawConnectionProvider`].
///
/// On `connect` it mints a fresh id, stashes a paired connection under it,
/// and returns canned tools (with the empty `<server>` token the real
/// transport emits, so the registry's rewrite is exercised).
struct BridgeMock {
    tools: Vec<McpToolDto>,
    server_metadata: Option<lingxi_core::host::McpServerMetadataDto>,
    resources: Vec<McpResourceDto>,
    prompts: Vec<McpPromptDto>,
    // §26a — canned `resources/templates/list` rows and the capability
    // presence bit that gates whether `connect` fetches them at all
    // (`initialize` reports `resources: true` only when this is set).
    resource_templates: Vec<lingxi_core::host::McpResourceTemplateDto>,
    resources_capability: AtomicBool,
    prompts_capability: AtomicBool,
    list_resource_templates_fails: AtomicBool,
    list_resources_fails: AtomicBool,
    list_prompts_fails: AtomicBool,
    drivable_calls: bool,
    modern_connect: bool,
    /// How many times `resources/templates/list` was actually issued. The
    /// parity claim is ZERO RPCs when the discovery cache is ineligible —
    /// an empty `resource_templates` field would also pass if we fetched
    /// and discarded, so the field alone cannot prove it.
    templates_calls: AtomicUsize,
    conns: TestMutex<HashMap<ConnId, Arc<Connection>>>,
    list_tools_fails: AtomicBool,
    block_list_tools: AtomicBool,
    list_tools_started: Notify,
    list_tools_release: Notify,
    disconnect_fails: AtomicBool,
    block_disconnect: AtomicBool,
    hang_disconnect: AtomicBool,
    disconnect_started: Notify,
    disconnect_release: Notify,
    /// §11 Stage 2 — how many times `McpTransport::connect` actually
    /// dialed. The whole claim of Stage 2 is that a cache hit dials ZERO
    /// times and a subsequent tool call dials exactly once; an unchanged
    /// `tools`/`resources` field on the served state would pass even if
    /// the mock dialed and the result were silently discarded, so the
    /// call COUNT is the only thing that actually proves it.
    connect_calls: AtomicUsize,
    block_connect: AtomicBool,
    panic_connect: AtomicBool,
    panic_initialize: AtomicBool,
    panic_list_tools: AtomicBool,
    connect_started: Notify,
    connect_release: Notify,
    block_resource_templates: AtomicBool,
    resource_templates_started: Notify,
    resource_templates_release: Notify,
    inbound_txs: TestMutex<HashMap<ConnId, mpsc::Sender<Bytes>>>,
    tool_call_peers: TestMutex<HashMap<ConnId, mpsc::Receiver<Bytes>>>,
    listen_writer_failures_remaining: AtomicUsize,
    connect_failures_remaining: AtomicUsize,
    connect_auth_status: AtomicUsize,
    disconnect_failures_remaining: AtomicUsize,
    /// §11 Stage 2 — how many times `McpTransport::disconnect` actually
    /// ran, so a test can prove a `Cached` server's teardown does NOT
    /// touch the transport (it has no live connection registered).
    disconnect_calls: AtomicUsize,
}

impl BridgeMock {
    fn new(tool_names: &[&str]) -> Self {
        let tools = tool_names
            .iter()
            .map(|t| McpToolDto {
                // Emit the empty `<server>` token, exactly like the posix
                // transport's `list_tools` (mcp.rs:397) does.
                full_name: format!("mcp____{t}"),
                server_name: String::new(),
                tool_name: (*t).to_string(),
                description: format!("{t} tool"),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            })
            .collect();
        Self {
            tools,
            server_metadata: None,
            resources: Vec::new(),
            prompts: Vec::new(),
            resource_templates: Vec::new(),
            resources_capability: AtomicBool::new(false),
            prompts_capability: AtomicBool::new(false),
            list_resource_templates_fails: AtomicBool::new(false),
            list_resources_fails: AtomicBool::new(false),
            list_prompts_fails: AtomicBool::new(false),
            drivable_calls: false,
            modern_connect: false,
            templates_calls: AtomicUsize::new(0),
            conns: TestMutex::new(HashMap::new()),
            list_tools_fails: AtomicBool::new(false),
            block_list_tools: AtomicBool::new(false),
            list_tools_started: Notify::new(),
            list_tools_release: Notify::new(),
            disconnect_fails: AtomicBool::new(false),
            block_disconnect: AtomicBool::new(false),
            hang_disconnect: AtomicBool::new(false),
            disconnect_started: Notify::new(),
            disconnect_release: Notify::new(),
            connect_calls: AtomicUsize::new(0),
            block_connect: AtomicBool::new(false),
            panic_connect: AtomicBool::new(false),
            panic_initialize: AtomicBool::new(false),
            panic_list_tools: AtomicBool::new(false),
            connect_started: Notify::new(),
            connect_release: Notify::new(),
            block_resource_templates: AtomicBool::new(false),
            resource_templates_started: Notify::new(),
            resource_templates_release: Notify::new(),
            inbound_txs: TestMutex::new(HashMap::new()),
            tool_call_peers: TestMutex::new(HashMap::new()),
            listen_writer_failures_remaining: AtomicUsize::new(0),
            connect_failures_remaining: AtomicUsize::new(0),
            connect_auth_status: AtomicUsize::new(0),
            disconnect_failures_remaining: AtomicUsize::new(0),
            disconnect_calls: AtomicUsize::new(0),
        }
    }

    fn with_drivable_calls(tool_names: &[&str]) -> Self {
        Self {
            drivable_calls: true,
            ..Self::new(tool_names)
        }
    }

    fn with_modern_drivable_calls(tool_names: &[&str]) -> Self {
        Self {
            drivable_calls: true,
            modern_connect: true,
            ..Self::new(tool_names)
        }
    }

    /// A mock whose server advertises the `resources` capability and
    /// answers `resources/templates/list` with `templates` (§26a).
    fn with_resource_templates(templates: Vec<lingxi_core::host::McpResourceTemplateDto>) -> Self {
        let mock = Self::new(&[]);
        mock.resources_capability.store(true, Ordering::SeqCst);
        Self {
            resource_templates: templates,
            ..mock
        }
    }

    fn with_catalogs(
        tool_names: &[&str],
        resources: Vec<McpResourceDto>,
        prompts: Vec<McpPromptDto>,
    ) -> Self {
        let mock = Self::new(tool_names);
        mock.resources_capability
            .store(!resources.is_empty(), Ordering::SeqCst);
        mock.prompts_capability
            .store(!prompts.is_empty(), Ordering::SeqCst);
        Self {
            resources,
            prompts,
            ..mock
        }
    }

    /// Same as [`Self::new`], but each tool carries a caller-supplied
    /// `inputSchema` so the §20a connect-path decision can be driven.
    fn with_tool_schemas(tools: &[(&str, serde_json::Value)]) -> Self {
        let mut mock = Self::new(&[]);
        mock.tools = tools
            .iter()
            .map(|(name, schema)| McpToolDto {
                input_schema_projection: None,
                definition_projection: None,

                full_name: format!("mcp____{name}"),
                server_name: String::new(),
                tool_name: (*name).to_string(),
                description: format!("{name} tool"),
                input_schema: schema.clone(),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            })
            .collect();
        mock
    }

    fn take_tool_call_peer(
        &self,
        id: ConnId,
    ) -> Option<(mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>)> {
        let inbound = self.inbound_txs.lock().unwrap().remove(&id)?;
        let peer = self.tool_call_peers.lock().unwrap().remove(&id)?;
        Some((inbound, peer))
    }
}

#[async_trait]
impl McpTransport for BridgeMock {
    async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        self.connect_started.notify_one();
        if self.block_connect.load(Ordering::SeqCst) {
            self.connect_release.notified().await;
        }
        assert!(
            !self.panic_connect.load(Ordering::SeqCst),
            "bridge mock forced panic in connect"
        );
        if self.connect_failures_remaining.load(Ordering::SeqCst) > 0 {
            let _ = self.connect_failures_remaining.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |remaining| (remaining > 0).then_some(remaining - 1),
            );
            return Err(McpError::Connection(
                "bridge mock forced connect failure".into(),
            ));
        }
        let auth_status = self.connect_auth_status.load(Ordering::SeqCst);
        if auth_status != 0 {
            return Err(McpError::HttpResponse {
                status: auth_status as u16,
                www_authenticate: None,
            });
        }
        let id = ConnId::new();
        if self.drivable_calls {
            let (connection, inbound_tx, peer_rx) = drivable_connection();
            self.inbound_txs.lock().unwrap().insert(id, inbound_tx);
            if self.listen_writer_failures_remaining.load(Ordering::SeqCst) > 0 {
                let _ = self.listen_writer_failures_remaining.fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |remaining| (remaining > 0).then_some(remaining - 1),
                );
            } else {
                self.tool_call_peers.lock().unwrap().insert(id, peer_rx);
            }
            self.conns.lock().unwrap().insert(id, connection);
        } else {
            self.conns.lock().unwrap().insert(id, paired_connection());
        }
        Ok(McpRawConnection { connection_id: id })
    }
    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        _options: lingxi_core::host::McpConnectOptions,
    ) -> Result<lingxi_core::host::McpConnectResult, McpError> {
        let connection = self.connect(spec).await?;
        let capabilities = match std::panic::AssertUnwindSafe(self.initialize(&connection))
            .catch_unwind()
            .await
        {
            Ok(Ok(capabilities)) => capabilities,
            Ok(Err(error)) => {
                let _ = self.disconnect(connection.connection_id).await;
                return Err(error);
            }
            Err(payload) => {
                let _ = self.disconnect(connection.connection_id).await;
                std::panic::resume_unwind(payload);
            }
        };
        let negotiated = if self.modern_connect {
            lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Modern,
                version: "2026-07-28".into(),
            }
        } else {
            lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            }
        };
        Ok(lingxi_core::host::McpConnectResult {
            connection,
            capabilities,
            negotiated,
        })
    }
    async fn initialize(&self, _c: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        assert!(
            !self.panic_initialize.load(Ordering::SeqCst),
            "bridge mock forced panic in initialize"
        );
        Ok(ServerCapabilitiesDto {
            tools: true,
            resources: self.resources_capability.load(Ordering::SeqCst),
            prompts: self.prompts_capability.load(Ordering::SeqCst),
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        })
    }
    async fn list_resource_templates(
        &self,
        _c: &McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpResourceTemplateDto>, McpError> {
        self.templates_calls.fetch_add(1, Ordering::SeqCst);
        self.resource_templates_started.notify_one();
        if self.block_resource_templates.load(Ordering::SeqCst) {
            self.resource_templates_release.notified().await;
        }
        if self.list_resource_templates_fails.load(Ordering::SeqCst) {
            // What a server with no `resources/templates/list` handler
            // really replies: JSON-RPC -32601, which the posix transport
            // flattens through `map_call_err` into `McpError::Internal`.
            return Err(McpError::Internal(
                "MCP error -32601: Method not found".into(),
            ));
        }
        Ok(self.resource_templates.clone())
    }
    async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        self.list_tools_started.notify_one();
        if self.block_list_tools.load(Ordering::SeqCst) {
            self.list_tools_release.notified().await;
        }
        assert!(
            !self.panic_list_tools.load(Ordering::SeqCst),
            "bridge mock forced panic in list_tools"
        );
        if self.list_tools_fails.load(Ordering::SeqCst) {
            return Err(McpError::Internal("list tools failed".into()));
        }
        Ok(self.tools.clone())
    }
    async fn list_resources(&self, _c: &McpRawConnection) -> Result<Vec<McpResourceDto>, McpError> {
        if self.list_resources_fails.load(Ordering::SeqCst) {
            return Err(McpError::Internal("list resources failed".into()));
        }
        Ok(self.resources.clone())
    }
    async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        if self.list_prompts_fails.load(Ordering::SeqCst) {
            return Err(McpError::Internal("list prompts failed".into()));
        }
        Ok(self.prompts.clone())
    }
    async fn call_tool(
        &self,
        _c: &McpRawConnection,
        _t: &str,
        _i: Value,
    ) -> Result<McpToolResultDto, McpError> {
        Ok(McpToolResultDto {
            result_projection: None,

            content: Value::Null,
            is_error: false,
            ..Default::default()
        })
    }
    async fn read_resource(
        &self,
        _c: &McpRawConnection,
        _u: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        Err(McpError::Internal("not implemented".into()))
    }
    async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
        Ok(())
    }
    async fn notifications(
        &self,
        _c: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        // Never exercised by these tests (the registry's connect path does
        // not subscribe to notifications).
        unreachable!("notifications not used by bridge/FQN tests")
    }
    async fn handle_elicitation(
        &self,
        _c: &McpRawConnection,
        _r: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Err(McpError::Internal("not implemented".into()))
    }
    async fn disconnect(&self, id: ConnId) -> Result<(), McpError> {
        self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
        self.disconnect_started.notify_one();
        if self.hang_disconnect.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.block_disconnect.load(Ordering::SeqCst) {
            self.disconnect_release.notified().await;
        }
        if self
            .disconnect_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                if remaining > 0 {
                    Some(remaining - 1)
                } else {
                    None
                }
            })
            .is_ok()
        {
            return Err(McpError::Internal("disconnect failed".into()));
        }
        if self.disconnect_fails.load(Ordering::SeqCst) {
            return Err(McpError::Internal("disconnect failed".into()));
        }
        self.inbound_txs.lock().unwrap().remove(&id);
        self.tool_call_peers.lock().unwrap().remove(&id);
        self.conns.lock().unwrap().remove(&id);
        Ok(())
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio]
    }
    fn server_metadata(&self, id: ConnId) -> Option<lingxi_core::host::McpServerMetadataDto> {
        self.conns
            .lock()
            .unwrap()
            .contains_key(&id)
            .then(|| self.server_metadata.clone())
            .flatten()
    }
}

impl RawConnectionProvider for BridgeMock {
    fn connection_for(&self, id: ConnId) -> Option<Arc<Connection>> {
        self.conns.lock().unwrap().get(&id).cloned()
    }
}

/// Same-process transport used to prove that an `InProcess` catalog is
/// callable without manufacturing a JSON-RPC client solely for dispatch.
struct DirectInProcessMock {
    connection_id: ConnId,
    calls: TestMutex<Vec<(String, Value)>>,
}

impl DirectInProcessMock {
    fn new() -> Self {
        Self {
            connection_id: ConnId::new(),
            calls: TestMutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl McpTransport for DirectInProcessMock {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        assert!(
            matches!(spec, McpTransportSpec::InProcess { registry_key } if registry_key == "local_apps")
        );
        Ok(McpRawConnection {
            connection_id: self.connection_id,
        })
    }

    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: true,
            resources: false,
            prompts: false,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        })
    }

    async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        Ok(vec![McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: String::new(),
            tool_name: "list".into(),
            description: "list local apps".into(),
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: String::new(),
            search_hint: None,
            always_load: Some(true),
            requires_user_interaction: false,
        }])
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
        tool: &str,
        input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.into(), input.clone()));
        Ok(McpToolResultDto {
            result_projection: None,

            content: serde_json::json!([{"type":"text","text":"ok"}]),
            structured_content: Some(serde_json::json!({"tool": tool, "input": input})),
            is_error: false,
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        Err(McpError::Internal("resources disabled".into()))
    }

    async fn ping(&self, _connection_id: ConnId) -> Result<(), McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!("direct registry connections do not subscribe to JSON-RPC notifications")
    }

    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Err(McpError::Internal("elicitation disabled".into()))
    }

    async fn disconnect(&self, _connection_id: ConnId) -> Result<(), McpError> {
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::InProcess]
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

type DiscoveryCacheEnvGuard = crate::discovery_cache::TestEnvGuard;

async fn apply_lagged_tool_recovery(
    registry: &McpRegistry,
    active: &mut std::collections::HashSet<McpConnectionId>,
) -> Result<(), McpError> {
    let snapshot = registry.catalog_refresh_snapshot().await;
    *active = snapshot
        .iter()
        .filter(|change| change.kind == McpCatalogKind::Tools)
        .map(|change| change.connection_id)
        .collect();
    for change in snapshot {
        if change.kind == McpCatalogKind::Tools {
            if let Some(connection_id) = registry.refresh_catalog(&change).await? {
                active.insert(connection_id);
            }
        }
    }
    Ok(())
}

async fn drive_cleanup_retry_attempts_for_test() {
    for delay_ms in [10, 20, 40, 80, 160] {
        tokio::time::advance(Duration::from_millis(delay_ms)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(cleanup_disconnect_timeout()).await;
        tokio::task::yield_now().await;
    }
}

fn spawn_tools_list_response(
    peer_tx: mpsc::Sender<Bytes>,
    mut peer_rx: mpsc::Receiver<Bytes>,
    tool_name: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("tools/list request within timeout")
            .expect("tools/list frame");
        let request: Value = serde_json::from_slice(&request_frame).expect("tools/list json");
        assert_eq!(request["method"], "tools/list");
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "tools": [{
                    "name": tool_name,
                    "description": "fresh",
                    "inputSchema": {"type": "object"}
                }]
            }
        }))
        .expect("tools/list response");
        response.push(b'\n');
        peer_tx
            .send(Bytes::from(response))
            .await
            .expect("send tools/list response");
    })
}

fn spawn_tool_call_response(
    peer_tx: mpsc::Sender<Bytes>,
    mut peer_rx: mpsc::Receiver<Bytes>,
    tool_name: &'static str,
    input: Value,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("tools/call request within timeout")
            .expect("tools/call frame");
        let request: Value = serde_json::from_slice(&request_frame).expect("tools/call json");
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], tool_name);
        assert_eq!(request["params"]["arguments"], input);
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "structuredContent": {"tool": tool_name, "input": request["params"]["arguments"].clone()},
                "isError": false
            }
        }))
        .expect("tools/call response");
        response.push(b'\n');
        peer_tx
            .send(Bytes::from(response))
            .await
            .expect("send tools/call response");
    })
}

fn spawn_tool_call_auth_then_success(
    mock: Arc<BridgeMock>,
    first_connection_id: Option<ConnId>,
    tool_name: &'static str,
    input: Value,
    auth_status: u16,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let first_connection_id = if let Some(id) = first_connection_id {
            id
        } else {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(id) = mock.conns.lock().unwrap().keys().next().copied() {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first live connection appears")
        };
        let (first_tx, mut first_rx) = mock
            .take_tool_call_peer(first_connection_id)
            .expect("first tool-call peer");
        let first_input = input.clone();
        let first_request = tokio::time::timeout(Duration::from_secs(2), async move {
            let request_frame = first_rx.recv().await.expect("first tools/call frame");
            let request: Value =
                serde_json::from_slice(&request_frame).expect("first tools/call json");
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], tool_name);
            assert_eq!(request["params"]["arguments"], first_input);
            request
        })
        .await
        .expect("first tools/call request");
        let mut first_response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": first_request["id"].clone(),
            "error": {
                "code": -32000,
                "message": format!(
                    "MCP_HTTP_STATUS={auth_status};WWW_AUTHENTICATE=Bearer realm=\"mcp\""
                )
            }
        }))
        .expect("auth error response");
        first_response.push(b'\n');
        first_tx
            .send(Bytes::from(first_response))
            .await
            .expect("send auth error response");

        let second_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(id) = mock
                    .conns
                    .lock()
                    .unwrap()
                    .keys()
                    .copied()
                    .find(|id| *id != first_connection_id)
                {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconnect publishes a second live connection");
        let (second_tx, second_rx) = mock
            .take_tool_call_peer(second_connection_id)
            .expect("second tool-call peer");
        spawn_tool_call_response(second_tx, second_rx, tool_name, input)
            .await
            .expect("second tools/call responder");
    })
}

fn spawn_tool_call_auth_then_auth(
    mock: Arc<BridgeMock>,
    first_connection_id: Option<ConnId>,
    tool_name: &'static str,
    input: Value,
    auth_status: u16,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let first_connection_id = if let Some(id) = first_connection_id {
            id
        } else {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(id) = mock.conns.lock().unwrap().keys().next().copied() {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first live connection appears")
        };
        let (first_tx, mut first_rx) = mock
            .take_tool_call_peer(first_connection_id)
            .expect("first tool-call peer");
        let first_input = input.clone();
        let first_request = tokio::time::timeout(Duration::from_secs(2), async move {
            let request_frame = first_rx.recv().await.expect("first tools/call frame");
            let request: Value =
                serde_json::from_slice(&request_frame).expect("first tools/call json");
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], tool_name);
            assert_eq!(request["params"]["arguments"], first_input);
            request
        })
        .await
        .expect("first tools/call request");
        let mut first_response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": first_request["id"].clone(),
            "error": {
                "code": -32000,
                "message": format!(
                    "MCP_HTTP_STATUS={auth_status};WWW_AUTHENTICATE=Bearer realm=\"mcp\""
                )
            }
        }))
        .expect("first auth error response");
        first_response.push(b'\n');
        first_tx
            .send(Bytes::from(first_response))
            .await
            .expect("send first auth error response");

        let second_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(id) = mock
                    .conns
                    .lock()
                    .unwrap()
                    .keys()
                    .copied()
                    .find(|id| *id != first_connection_id)
                {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconnect publishes a second live connection");
        let (second_tx, mut second_rx) = mock
            .take_tool_call_peer(second_connection_id)
            .expect("second tool-call peer");
        let second_request = tokio::time::timeout(Duration::from_secs(2), async move {
            let request_frame = second_rx.recv().await.expect("second tools/call frame");
            let request: Value =
                serde_json::from_slice(&request_frame).expect("second tools/call json");
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], tool_name);
            assert_eq!(request["params"]["arguments"], input);
            request
        })
        .await
        .expect("second tools/call request");
        let mut second_response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": second_request["id"].clone(),
            "error": {
                "code": -32000,
                "message": format!(
                    "MCP_HTTP_STATUS={auth_status};WWW_AUTHENTICATE=Bearer realm=\"mcp\""
                )
            }
        }))
        .expect("second auth error response");
        second_response.push(b'\n');
        second_tx
            .send(Bytes::from(second_response))
            .await
            .expect("send second auth error response");
    })
}

fn spawn_tool_call_session_expired_then_success(
    mock: Arc<BridgeMock>,
    first_connection_id: Option<ConnId>,
    tool_name: &'static str,
    input: Value,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let first_connection_id = if let Some(id) = first_connection_id {
            id
        } else {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(id) = mock.conns.lock().unwrap().keys().next().copied() {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first live connection appears")
        };
        let (first_tx, mut first_rx) = mock
            .take_tool_call_peer(first_connection_id)
            .expect("first tool-call peer");
        let first_input = input.clone();
        let first_request = tokio::time::timeout(Duration::from_secs(2), async move {
            let request_frame = first_rx.recv().await.expect("first tools/call frame");
            let request: Value =
                serde_json::from_slice(&request_frame).expect("first tools/call json");
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], tool_name);
            assert_eq!(request["params"]["arguments"], first_input);
            request
        })
        .await
        .expect("first tools/call request");
        let mut first_response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": first_request["id"].clone(),
            "error": {
                "code": -32001,
                "message": "MCP_HTTP_STATUS=404;WWW_AUTHENTICATE="
            }
        }))
        .expect("session expired response");
        first_response.push(b'\n');
        first_tx
            .send(Bytes::from(first_response))
            .await
            .expect("send session expired response");

        let second_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(id) = mock
                    .conns
                    .lock()
                    .unwrap()
                    .keys()
                    .copied()
                    .find(|id| *id != first_connection_id)
                {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconnect publishes a second live connection");
        let (second_tx, second_rx) = mock
            .take_tool_call_peer(second_connection_id)
            .expect("second tool-call peer");
        spawn_tool_call_response(second_tx, second_rx, tool_name, input)
            .await
            .expect("second tools/call responder");
    })
}

fn cfg(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::Stdio {
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

/// [`cfg`] with a remote `http` spec, so the §20a per-server gate has a
/// hostname to resolve.
fn http_cfg(name: &str, url: &str) -> McpServerConfig {
    McpServerConfig {
        spec: McpTransportSpec::Http {
            url: url.into(),
            headers: Default::default(),
            headers_helper: None,
            oauth: None,
        },
        ..cfg(name)
    }
}

fn resource(name: &str, uri: &str) -> McpResourceDto {
    McpResourceDto {
        uri: uri.into(),
        name: name.into(),
        description: None,
        mime_type: None,
        meta: None,
    }
}

fn prompt(name: &str) -> McpPromptDto {
    McpPromptDto {
        name: name.into(),
        description: None,
        arguments: Vec::new(),
    }
}

fn legacy_negotiated() -> lingxi_core::host::McpNegotiatedProtocol {
    lingxi_core::host::McpNegotiatedProtocol {
        era: lingxi_core::host::McpProtocolEra::Legacy,
        version: "2025-11-25".into(),
    }
}

fn modern_negotiated() -> lingxi_core::host::McpNegotiatedProtocol {
    lingxi_core::host::McpNegotiatedProtocol {
        era: lingxi_core::host::McpProtocolEra::Modern,
        version: "2026-07-28".into(),
    }
}

fn caps(tools: bool, resources: bool, prompts: bool) -> ServerCapabilitiesDto {
    ServerCapabilitiesDto {
        tools,
        resources,
        prompts,
        directory_read: false,
        logging: false,
        experimental: HashMap::new(),
        extensions: HashMap::new(),
    }
}

// ---- Batch 1: client bridge --------------------------------------------

#[tokio::test]
async fn connect_registers_client_when_raw_conn_present() {
    let tool_names: Vec<String> = (0..17).map(|index| format!("blocked-{index}")).collect();
    let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    let mock = Arc::new(BridgeMock::new(&tool_name_refs));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("mock")).await.unwrap();
    assert!(
        registry.get_client("mock").await.is_some(),
        "a live McpClient must be registered when raw_conn is present"
    );
}

#[tokio::test]
async fn conditional_remove_requires_the_expected_config_snapshot() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let expected = http_cfg("srv", "https://mcp.example.com/old");
    let newer = http_cfg("srv", "https://mcp.example.com/new");
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Disconnected {
            config: newer.clone(),
            last_error: None,
        },
    );

    assert!(!registry
        .remove_without_revoking_auth_if_config("srv", &expected)
        .await
        .expect("conditional removal mismatch must be observable"));
    assert!(registry.connections.read().await.contains_key("srv"));

    assert!(registry
        .remove_without_revoking_auth_if_config("srv", &newer)
        .await
        .expect("matching conditional removal must succeed"));
    assert!(!registry.connections.read().await.contains_key("srv"));
}

#[tokio::test]
async fn guarded_conditional_remove_rechecks_generation_after_lifecycle_wait() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
    let expected = http_cfg("srv", "https://mcp.example.com/v1");
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Disconnected {
            config: expected.clone(),
            last_error: None,
        },
    );

    let lifecycle = registry.lifecycle_lock("srv");
    let held = lifecycle.lock().await;
    let current = Arc::new(AtomicBool::new(true));
    let guard_calls = Arc::new(AtomicUsize::new(0));
    let guard = {
        let current = current.clone();
        let guard_calls = guard_calls.clone();
        Arc::new(move || {
            guard_calls.fetch_add(1, Ordering::SeqCst);
            current.load(Ordering::SeqCst)
        }) as Arc<McpOperationGuard>
    };
    let removal = {
        let registry = registry.clone();
        let guard = guard.clone();
        tokio::spawn(async move {
            registry
                .remove_without_revoking_auth_if_config_guarded("srv", &expected, guard)
                .await
        })
    };

    // The job is queued behind the lifecycle lock. Invalidate its
    // generation before releasing that lock; the registry callback must
    // run after lock acquisition and prevent the stale removal.
    tokio::task::yield_now().await;
    current.store(false, Ordering::SeqCst);
    drop(held);
    assert!(!removal
        .await
        .expect("guarded removal task")
        .expect("guarded removal result"));
    assert_eq!(guard_calls.load(Ordering::SeqCst), 1);
    assert!(registry.connections.read().await.contains_key("srv"));
}

#[tokio::test]
async fn guarded_conditional_remove_rechecks_generation_after_snapshot_wait() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
    let expected = http_cfg("srv", "https://mcp.example.com/v1");
    let mut held = registry.connections.write().await;
    held.insert(
        "srv".into(),
        McpConnectionState::Disconnected {
            config: expected.clone(),
            last_error: None,
        },
    );
    let current = Arc::new(AtomicBool::new(true));
    let guard_calls = Arc::new(AtomicUsize::new(0));
    let guard = {
        let current = current.clone();
        let guard_calls = guard_calls.clone();
        Arc::new(move || {
            guard_calls.fetch_add(1, Ordering::SeqCst);
            current.load(Ordering::SeqCst)
        }) as Arc<McpOperationGuard>
    };
    let removal = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .remove_without_revoking_auth_if_config_guarded("srv", &expected, guard)
                .await
        })
    };
    // Wait for the guard to pass under the lifecycle lock, then revoke
    // intent while its config snapshot is blocked by our state writer.
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while guard_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("removal reached the snapshot wait");
    current.store(false, Ordering::SeqCst);
    drop(held);
    assert!(!removal
        .await
        .expect("removal task")
        .expect("removal result"));
    assert!(registry.connections.read().await.contains_key("srv"));
}

#[tokio::test]
async fn guarded_connect_rejects_before_live_publish_and_disconnects_discovery() {
    let publish_hook = Arc::new(TestPauseHook::default());
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = Arc::new(
        McpRegistry::new(mock.clone() as Arc<dyn McpTransport>)
            .with_pause_before_client_publish(publish_hook.clone()),
    );
    let current = Arc::new(AtomicBool::new(true));
    let guard = {
        let current = current.clone();
        Arc::new(move || current.load(Ordering::SeqCst)) as Arc<McpOperationGuard>
    };

    let connect = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.connect_if_current(cfg("srv"), guard).await })
    };
    tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
        .await
        .expect("guarded connect reaches transport");
    tokio::time::timeout(Duration::from_secs(2), publish_hook.entered.notified())
        .await
        .expect("guarded connect reaches the publish gate");
    current.store(false, Ordering::SeqCst);
    publish_hook.release.notify_one();

    assert!(connect
        .await
        .expect("guarded connect task")
        .expect("guarded connect result")
        .is_none());
    assert!(registry.connections.read().await.is_empty());
    assert!(mock.conns.lock().unwrap().is_empty());
    assert_eq!(mock.disconnect_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_guarded_connect_does_not_remove_a_newer_disconnected_config() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let desired = cfg("srv");
    let newer = http_cfg("srv", "https://mcp.example.com/newer");
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Disconnected {
            config: newer.clone(),
            last_error: None,
        },
    );
    let guard = Arc::new(|| false) as Arc<McpOperationGuard>;

    assert!(registry
        .connect_if_current(desired, guard)
        .await
        .expect("rejected connect result")
        .is_none());
    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Disconnected { config, .. })
            if McpRegistry::same_config_snapshot(config, &newer)
    ));
}

#[tokio::test]
async fn stale_guarded_connect_error_does_not_publish_disconnected_state() {
    let mock = Arc::new(BridgeMock::new(&[]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(McpRegistry::new(mock.clone() as Arc<dyn McpTransport>));
    let current = Arc::new(AtomicBool::new(true));
    let guard = {
        let current = current.clone();
        Arc::new(move || current.load(Ordering::SeqCst)) as Arc<McpOperationGuard>
    };
    let connect = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.connect_if_current(cfg("srv"), guard).await })
    };
    tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
        .await
        .expect("blocked connect reaches transport");

    // The transport fails only after this generation is superseded. The
    // stale error path must clean its own Connecting marker, not publish
    // a Disconnected state carrying the old config.
    current.store(false, Ordering::SeqCst);
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();

    assert!(connect
        .await
        .expect("stale connect task")
        .expect("stale connect result")
        .is_none());
    assert!(registry.connections.read().await.is_empty());
    assert!(mock.conns.lock().unwrap().is_empty());
}

#[tokio::test]
async fn guarded_disabled_connect_seeds_disconnected_without_dialing() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock.clone() as Arc<dyn McpTransport>);
    let mut disabled = cfg("srv");
    disabled.disabled = true;
    let guard = Arc::new(|| true) as Arc<McpOperationGuard>;

    assert!(registry
        .connect_if_current(disabled.clone(), guard)
        .await
        .expect("disabled guarded connect")
        .is_none());
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Disconnected { config, last_error: None })
            if config.disabled && McpRegistry::same_config_snapshot(config, &disabled)
    ));
}

#[tokio::test]
async fn inprocess_server_dispatches_directly_without_jsonrpc_client() {
    let transport = Arc::new(DirectInProcessMock::new());
    let registry = McpRegistry::new(transport.clone());
    registry
        .connect(McpServerConfig {
            name: "local_apps".into(),
            spec: McpTransportSpec::InProcess {
                registry_key: "local_apps".into(),
            },
            scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
            disabled: false,
            timeout_ms: None,
            always_load: true,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();

    assert!(registry.get_client("local_apps").await.is_none());
    assert!(registry.has_callable_server("local_apps").await);
    let result = registry
        .call_tool_with_auth_retry(
            "local_apps",
            "mcp__local_apps__list",
            lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!({"limit": 5})),
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({"tool":"list","input":{"limit":5}}))
    );
    assert_eq!(
        *transport.calls.lock().unwrap(),
        vec![("list".into(), serde_json::json!({"limit": 5}))]
    );
}

#[tokio::test]
async fn tools_list_failure_keeps_transport_connected_and_emits_success_before_catalogs_finish() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mock = Arc::new(BridgeMock::new(&["read"]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    mock.block_list_tools.store(true, Ordering::SeqCst);
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));

    let connect = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.connect(cfg("mock")).await })
    };
    tokio::time::timeout(Duration::from_secs(2), mock.list_tools_started.notified())
        .await
        .expect("connect must reach list_tools");

    let midflight = take_test_telemetry_events();
    assert!(
        midflight
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_SUCCEEDED),
        "initialize success must emit before catalog completion"
    );
    assert!(
        !midflight
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::TOOLS_LISTED
                && event.payload.get("tool_count") == Some(&serde_json::json!(17))),
        "tools/list event must wait for the catalog result"
    );

    mock.list_tools_release.notify_one();
    let connection_id = connect.await.expect("join").expect("connect succeeds");
    let tail_events = take_test_telemetry_events();
    let mut events = midflight.clone();
    events.extend(tail_events);
    assert!(
        events
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_SUCCEEDED),
        "successful initialize must be recorded"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_FAILED),
        "catalog failure must not be reported as a connection failure"
    );
    assert!(
        events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::DEGRADED
                && event.payload.get("reason") == Some(&serde_json::json!("tools_list_failed"))
        }),
        "tools/list failure must emit the exact degraded reason"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::TOOLS_LISTED
                && event.payload.get("tool_count") == Some(&serde_json::json!(17))),
        "failed tools/list must not emit tools_listed"
    );
    let states = registry.connections.read().await;
    let Some(McpConnectionState::Connected {
        connection_id: current,
        tools,
        ..
    }) = states.get("mock")
    else {
        panic!("expected Connected state after partial catalog failure");
    };
    assert_eq!(*current, connection_id);
    assert!(tools.is_empty(), "failed tools catalog falls back to empty");
    assert!(
        mock.conns.lock().unwrap().contains_key(&connection_id),
        "the initialized transport must remain live"
    );
}

#[tokio::test]
async fn resources_list_failure_keeps_tools_and_marks_only_resources_failed() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mock = Arc::new(BridgeMock::with_catalogs(
        &["read"],
        vec![resource("guide", "file:///guide.md")],
        Vec::new(),
    ));
    mock.resources_capability.store(true, Ordering::SeqCst);
    mock.list_resources_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );

    registry
        .connect(cfg("mock"))
        .await
        .expect("connect succeeds");

    let states = registry.connections.read().await;
    let Some(McpConnectionState::Connected {
        tools, resources, ..
    }) = states.get("mock")
    else {
        panic!("expected Connected state");
    };
    assert_eq!(tools.len(), 1, "successful tools catalog is retained");
    assert!(
        resources.is_empty(),
        "failed resources catalog falls back to empty on first connect"
    );
    drop(states);

    let events = take_test_telemetry_events();
    assert!(
        events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::DEGRADED
                && event.payload.get("reason") == Some(&serde_json::json!("resources_list_failed"))
        }),
        "resources/list failure must emit the exact degraded reason"
    );
    assert!(
        events
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::TOOLS_LISTED),
        "successful tools/list must still emit tools_listed"
    );
}

#[tokio::test]
async fn prompts_list_failure_keeps_other_catalogs_live() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mock = Arc::new(BridgeMock::with_catalogs(
        &["read"],
        vec![resource("guide", "file:///guide.md")],
        vec![prompt("draft")],
    ));
    mock.list_prompts_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );

    registry
        .connect(cfg("mock"))
        .await
        .expect("connect succeeds");

    let states = registry.connections.read().await;
    let Some(McpConnectionState::Connected {
        tools,
        resources,
        prompts,
        ..
    }) = states.get("mock")
    else {
        panic!("expected Connected state");
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(resources.len(), 1);
    assert!(
        prompts.is_empty(),
        "failed prompts catalog falls back to empty"
    );
    drop(states);

    let events = take_test_telemetry_events();
    assert!(
        events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::DEGRADED
                && event.payload.get("reason") == Some(&serde_json::json!("prompts_list_failed"))
        }),
        "prompts/list failure must emit the exact degraded reason"
    );
}

#[tokio::test]
async fn mixed_catalog_failures_keep_successful_catalogs_and_preserve_cached_failed_slices() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry_with_catalog(
        &store,
        &cache_key,
        1_000_000,
        ServerCapabilitiesDto {
            tools: true,
            resources: true,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: "srv".into(),
            tool_name: "cached_tool".into(),
            description: "cached tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: "mcp__srv__cached_tool".into(),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        }],
        vec![resource("cached_guide", "file:///cached.md")],
        vec![prompt("cached_prompt")],
    );

    let mock = Arc::new(BridgeMock::with_catalogs(
        &["live_tool"],
        vec![resource("live_guide", "file:///live.md")],
        vec![prompt("live_prompt")],
    ));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    mock.list_prompts_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry
        .connect(cfg.clone())
        .await
        .expect("cache hit connect");
    let live_id = registry
        .ensure_dialed_from_cache("srv")
        .await
        .expect("partial live discovery still succeeds");
    assert_ne!(live_id, cached_id);

    let states = registry.connections.read().await;
    let Some(McpConnectionState::Connected {
        connection_id,
        tools,
        resources,
        prompts,
        ..
    }) = states.get("srv")
    else {
        panic!("expected Connected state after lazy dial");
    };
    assert_eq!(*connection_id, live_id);
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect::<Vec<_>>(),
        vec!["cached_tool"],
        "failed tools/list must preserve the cached safe catalog"
    );
    assert_eq!(
        resources
            .iter()
            .map(|resource| resource.name.as_str())
            .collect::<Vec<_>>(),
        vec!["live_guide"],
        "successful resources/list must replace the cached catalog"
    );
    assert_eq!(
        prompts
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>(),
        vec!["cached_prompt"],
        "failed prompts/list must preserve the cached safe catalog"
    );
    drop(states);
    drop(env);

    let events = take_test_telemetry_events();
    let degraded_reasons: Vec<&str> = events
        .iter()
        .filter(|event| event.name == telemetry::tengu::mcp::DEGRADED)
        .filter_map(|event| {
            event
                .payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    assert!(degraded_reasons.contains(&"tools_list_failed"));
    assert!(degraded_reasons.contains(&"prompts_list_failed"));
    assert!(
        !events
            .iter()
            .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_FAILED),
        "partial live discovery must not demote the initialized connection to a failure"
    );
}

#[tokio::test]
async fn connect_and_disconnect_publish_tool_partition_lifecycle() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    let mut changes = registry.subscribe_catalog_changes();

    let connection_id = registry.connect(cfg("mock")).await.unwrap();
    let connected = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("connect catalog event within timeout")
        .expect("catalog sender remains live");
    assert_eq!(
        connected,
        McpCatalogChanged {
            server_name: "mock".into(),
            connection_id,
            retired_connection_id: None,
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        }
    );

    registry.disconnect("mock").await.unwrap();
    let disconnected = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("disconnect catalog event within timeout")
        .expect("catalog sender remains live");
    assert_eq!(
        disconnected,
        McpCatalogChanged {
            server_name: "mock".into(),
            connection_id,
            retired_connection_id: Some(connection_id),
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        }
    );
}

#[tokio::test]
async fn lazy_dial_success_retires_cached_partition_then_disconnect_retires_live_partition() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    let mut changes = registry.subscribe_catalog_changes();

    let cached_id = registry.connect(cfg).await.expect("cache hit connect");
    let cached_connected = changes.recv().await.expect("cached connect event");
    assert_eq!(
        cached_connected,
        McpCatalogChanged {
            server_name: "srv".into(),
            connection_id: cached_id,
            retired_connection_id: None,
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        }
    );

    let live_id = registry
        .ensure_dialed_from_cache("srv")
        .await
        .expect("foreground lazy dial");
    let live_connected = changes.recv().await.expect("live replacement event");
    assert_eq!(
        live_connected,
        McpCatalogChanged {
            server_name: "srv".into(),
            connection_id: live_id,
            retired_connection_id: Some(cached_id),
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        }
    );

    registry.disconnect("srv").await.expect("disconnect");
    let disconnected = changes.recv().await.expect("disconnect event");
    assert_eq!(
        disconnected,
        McpCatalogChanged {
            server_name: "srv".into(),
            connection_id: live_id,
            retired_connection_id: Some(live_id),
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        }
    );
    drop(env);
}

#[tokio::test]
async fn lazy_dial_partial_catalog_failure_replaces_cached_partition_without_zombies() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    let mut changes = registry.subscribe_catalog_changes();

    let cached_id = registry.connect(cfg).await.expect("cache hit connect");
    let _ = changes.recv().await.expect("cached connect event");
    let live_id = registry
        .ensure_dialed_from_cache("srv")
        .await
        .expect("partial catalog failure must still promote the live transport");
    let replaced = changes.recv().await.expect("live replacement event");
    assert_eq!(
        replaced,
        McpCatalogChanged {
            server_name: "srv".into(),
            connection_id: live_id,
            retired_connection_id: Some(cached_id),
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), changes.recv())
            .await
            .is_err(),
        "partial lazy-dial replacement must publish exactly one follow-up event"
    );
    drop(env);
}

#[tokio::test]
async fn late_waiters_join_one_detached_cached_lazy_upgrade_owner() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let first = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_connected_client("srv").await })
    };
    mock.connect_started.notified().await;

    let second = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_connected_client("srv").await })
    };
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connecting { .. })
        ),
        "late waiter must join the detached lazy-upgrade owner while state is Connecting"
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let first_client = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("first waiter finishes")
        .expect("first join succeeds")
        .expect("first waiter receives client");
    let second_client = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("second waiter finishes")
        .expect("second join succeeds")
        .expect("second waiter receives client");
    drop(env);

    assert!(
        Arc::ptr_eq(&first_client, &second_client),
        "both waiters must receive the same published live client"
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "late waiters must share one lazy-upgrade dial"
    );
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { .. })
        ),
        "the detached owner must publish a final Connected state"
    );
}

#[tokio::test]
async fn cancelling_the_initiating_waiter_does_not_cancel_the_detached_lazy_upgrade_owner() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_connected_client("srv").await })
    };
    mock.connect_started.notified().await;
    waiter.abort();
    let join = waiter.await;
    assert!(join.is_err_and(|error| error.is_cancelled()));

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let later_client = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match registry.ensure_connected_client("srv").await {
                Ok(client) => break client,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("later waiter succeeds after detached owner publishes");
    drop(env);

    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "cancelling the initiating waiter must not trigger a second lazy-upgrade dial"
    );
    assert!(
        registry.lazy_upgrade_slots.read().await.is_empty(),
        "completed detached owner must retire its coordination slot"
    );
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { .. })
        ),
        "owner completion must not strand the server in Connecting after waiter cancellation"
    );
    assert!(
        registry.get_client("srv").await.is_some() && Arc::strong_count(&later_client) >= 1,
        "the published live client remains available after the initiating waiter is dropped"
    );
}

#[tokio::test]
async fn disconnecting_a_foreground_lazy_upgrade_clears_connecting_and_unblocks_waiters() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_connected_client("srv").await })
    };
    mock.connect_started.notified().await;

    registry.disconnect("srv").await.expect("disconnect wins");
    let waiter_result = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter finishes")
        .expect("join succeeds");
    let waiter_result = match waiter_result {
        Ok(_) => panic!("waiter should fail after disconnect"),
        Err(error) => error,
    };
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Stopped { .. })
        ),
        "disconnect must not leave a slot-owned Connecting zombie behind"
    );
    assert!(
        registry.lazy_upgrade_slots.read().await.is_empty(),
        "disconnect must remove the lazy-upgrade slot"
    );
    assert!(
        waiter_result
            .to_string()
            .contains("MCP server \"srv\" is no longer cached"),
        "disconnected waiters must resolve with a terminal error"
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Stopped { .. })
        ),
        "late owner completion must not revive the disconnected server"
    );
    drop(env);
}

#[tokio::test]
async fn removing_a_foreground_lazy_upgrade_drops_state_and_unblocks_waiters() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_connected_client("srv").await })
    };
    mock.connect_started.notified().await;

    registry
        .remove_without_revoking_auth("srv")
        .await
        .expect("remove wins");
    let waiter_result = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter finishes")
        .expect("join succeeds");
    let waiter_result = match waiter_result {
        Ok(_) => panic!("waiter should fail after remove"),
        Err(error) => error,
    };
    assert!(
        registry.connections.read().await.get("srv").is_none(),
        "remove must drop the slot-owned Connecting state entirely"
    );
    assert!(
        waiter_result
            .to_string()
            .contains("MCP server \"srv\" is no longer cached"),
        "removed waiters must resolve with a terminal error"
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        registry.connections.read().await.get("srv").is_none(),
        "late owner completion must not recreate a removed server"
    );
    drop(env);
}

#[tokio::test]
async fn failed_disconnect_preserves_live_state_client_and_catalog() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    let mut changes = registry.subscribe_catalog_changes();

    let connection_id = registry.connect(cfg("mock")).await.unwrap();
    changes.recv().await.unwrap();
    mock.disconnect_fails.store(true, Ordering::SeqCst);

    let error = registry.disconnect("mock").await.unwrap_err();
    assert!(error.to_string().contains("disconnect failed"));
    assert!(registry.get_client("mock").await.is_some());
    let conns = registry.connections.read().await;
    assert!(matches!(
        conns.get("mock"),
        Some(McpConnectionState::Connected {
            connection_id: current,
            ..
        }) if *current == connection_id
    ));
    drop(conns);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), changes.recv())
            .await
            .is_err(),
        "a failed teardown must not retire the live tool partition"
    );
}

#[tokio::test]
async fn live_disable_retires_connection_without_revoking_reconnect_config() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    let mut changes = registry.subscribe_catalog_changes();
    let connection_id = registry.connect(cfg("mock")).await.unwrap();
    changes.recv().await.unwrap();

    assert_eq!(
        registry.set_disabled("mock", true).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Disabled)
    );
    assert!(registry.get_client("mock").await.is_none());
    assert_eq!(
        registry.action_states().await,
        vec![(
            "mock".to_string(),
            lingxi_core::host::McpActionState::Disabled
        )]
    );
    let retired = changes.recv().await.unwrap();
    assert_eq!(retired.retired_connection_id, Some(connection_id));

    assert_eq!(
        registry.set_disabled("mock", false).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Connected)
    );
    assert!(registry.get_client("mock").await.is_some());
    assert_eq!(
        registry.action_states().await,
        vec![(
            "mock".to_string(),
            lingxi_core::host::McpActionState::Connected
        )]
    );
}

#[tokio::test]
async fn cached_disable_retires_connection_without_touching_transport() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    let mut changes = registry.subscribe_catalog_changes();

    let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
    let initial = changes.recv().await.expect("cached registration");
    assert_eq!(initial.connection_id, cached_id);
    assert_eq!(initial.retired_connection_id, None);

    assert_eq!(
        registry.set_disabled("srv", true).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Disabled)
    );
    assert_eq!(
        mock.disconnect_calls.load(Ordering::SeqCst),
        0,
        "cached disable must not call transport disconnect"
    );
    let retired = changes.recv().await.expect("cached retirement");
    assert_eq!(retired.retired_connection_id, Some(cached_id));
    drop(env);
}

#[tokio::test]
async fn failed_live_disable_preserves_connected_generation() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    let connection_id = registry.connect(cfg("mock")).await.unwrap();
    mock.disconnect_fails.store(true, Ordering::SeqCst);

    assert!(registry.set_disabled("mock", true).await.is_err());
    assert!(registry.get_client("mock").await.is_some());
    let conns = registry.connections.read().await;
    assert!(matches!(
        conns.get("mock"),
        Some(McpConnectionState::Connected {
            connection_id: current,
            config,
            ..
        }) if *current == connection_id && !config.disabled
    ));
}

#[tokio::test]
async fn failed_atomic_config_replacement_preserves_connected_generation() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    let original = cfg("mock");
    let connection_id = registry.connect(original.clone()).await.unwrap();
    mock.connect_failures_remaining.store(1, Ordering::SeqCst);
    let mut replacement = original.clone();
    replacement.timeout_ms = Some(42_000);

    assert!(registry
        .replace_config_atomically(replacement)
        .await
        .is_err());
    assert!(registry.get_client("mock").await.is_some());
    let connections = registry.connections.read().await;
    assert!(matches!(
        connections.get("mock"),
        Some(McpConnectionState::Connected {
            connection_id: current,
            config,
            ..
        }) if *current == connection_id && McpRegistry::same_config_snapshot(config, &original)
    ));
}

#[tokio::test]
async fn set_disabled_noop_false_preserves_a_foreground_lazy_upgrade_slot() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("fresh cache hit");
    let owner = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    mock.connect_started.notified().await;

    let slot = registry
        .lazy_upgrade_slot("srv")
        .await
        .expect("foreground slot registered");
    assert_eq!(registry.set_disabled("srv", false).await.unwrap(), None);
    let same_slot = registry
        .lazy_upgrade_slot("srv")
        .await
        .expect("no-op disable keeps foreground slot");
    assert!(Arc::ptr_eq(&slot, &same_slot));

    let joiner = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("no-op disable must not redial");

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let owner_id = tokio::time::timeout(Duration::from_secs(2), owner)
        .await
        .expect("owner waiter completes")
        .expect("owner join succeeds")
        .expect("owner receives live id");
    let joiner_id = tokio::time::timeout(Duration::from_secs(2), joiner)
        .await
        .expect("joiner completes")
        .expect("joiner task succeeds")
        .expect("joiner receives same live id");
    drop(env);

    assert_eq!(owner_id, joiner_id);
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn set_disabled_noop_false_preserves_a_background_lazy_upgrade_slot() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry.connect(cfg).await.expect("stale cache hit");
    mock.connect_started.notified().await;

    let slot = registry
        .lazy_upgrade_slot("srv")
        .await
        .expect("background slot registered");
    assert_eq!(registry.set_disabled("srv", false).await.unwrap(), None);
    let same_slot = registry
        .lazy_upgrade_slot("srv")
        .await
        .expect("no-op disable keeps background slot");
    assert!(Arc::ptr_eq(&slot, &same_slot));

    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground join must share the existing background owner");

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let live_id = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("foreground waiter completes")
        .expect("waiter join succeeds")
        .expect("foreground waiter receives live id");
    drop(env);

    assert_ne!(live_id, cached_id);
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disabling_a_foreground_lazy_upgrade_invalidates_the_slot_and_prevents_revival() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("fresh cache hit");
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    mock.connect_started.notified().await;

    assert_eq!(
        registry.set_disabled("srv", true).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Disabled)
    );
    assert!(
        registry.lazy_upgrade_slots.read().await.is_empty(),
        "disable must invalidate the foreground slot"
    );
    tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter completes after disable")
        .expect("waiter join succeeds")
        .unwrap_err();

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.disconnect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejected live transport cleaned up after disable");
    drop(env);

    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Disconnected { config, .. }) if config.disabled
    ));
}

#[tokio::test]
async fn enabling_a_server_that_with_a_catalog_failure_still_settles_connected() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("mock")).await.unwrap();
    // Disabling settles as `Disabled`.
    assert_eq!(
        registry.set_disabled("mock", true).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Disabled)
    );
    // The next connect still succeeds: the transport/initialize path is
    // authoritative, while tools/list now degrades in place.
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    // Re-enabling must still NOT return `Err`, and with independent
    // catalog fetches it now settles `Connected` with an empty tools slice.
    assert_eq!(
        registry.set_disabled("mock", false).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Connected)
    );
    assert_eq!(
        registry.action_states().await,
        vec![(
            "mock".to_string(),
            lingxi_core::host::McpActionState::Connected
        )]
    );
    assert!(matches!(
        registry.connections.read().await.get("mock"),
        Some(McpConnectionState::Connected { tools, .. }) if tools.is_empty()
    ));
}

#[tokio::test]
async fn failed_action_servers_reports_sanitized_errors_sorted() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    {
        let mut conns = registry.connections.write().await;
        conns.insert(
            "zeta".into(),
            McpConnectionState::Failed {
                config: cfg("zeta"),
                // The quotes must be stripped by the `xLt` sanitizer.
                error: "he said \"boom\"".into(),
                attempts: 3,
            },
        );
        conns.insert(
            "alpha".into(),
            McpConnectionState::Failed {
                config: cfg("alpha"),
                error: "Blocked by enterprise managed policy".into(),
                attempts: 1,
            },
        );
    }
    // Sorted by name; each error run through `sanitize_diagnostic`.
    assert_eq!(
        registry.failed_action_servers().await,
        vec![
            (
                "alpha".to_string(),
                Some("Blocked by enterprise managed policy".to_string())
            ),
            ("zeta".to_string(), Some("he said boom".to_string())),
        ]
    );
}

#[test]
fn sanitize_diagnostic_strips_quotes_controls_and_caps_at_200() {
    // Angle brackets, `"`, `;` and the fancy-quote set collapse to spaces.
    assert_eq!(sanitize_diagnostic("hello \"world\""), "hello world");
    assert_eq!(sanitize_diagnostic("angle <b> ; semi"), "angle b semi");
    // Control chars (`\p{Cc}`) collapse to a single space.
    assert_eq!(sanitize_diagnostic("a\u{0000}\u{0007}b"), "a b");
    // Runs of whitespace collapse and the result is trimmed.
    assert_eq!(sanitize_diagnostic("  trim   me  "), "trim me");
    // Capped at 200 chars + `…` (U+2026).
    let capped = sanitize_diagnostic(&"x".repeat(250));
    assert_eq!(capped.chars().count(), 201);
    assert!(capped.ends_with('\u{2026}'));
}

#[tokio::test]
async fn slow_disconnect_does_not_block_registry_snapshots() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    registry.connect(cfg("mock")).await.unwrap();
    mock.block_disconnect.store(true, Ordering::SeqCst);

    let disconnect = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.disconnect("mock").await })
    };
    mock.disconnect_started.notified().await;

    let snapshot = tokio::time::timeout(Duration::from_millis(50), registry.snapshot())
        .await
        .expect("transport teardown must not hold the connection-state lock");
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].status, lingxi_core::host::McpStatus::Connected);

    mock.disconnect_release.notify_one();
    disconnect.await.unwrap().unwrap();
}

#[tokio::test]
async fn reconnect_one_does_not_clobber_an_already_connected_server() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    let id1 = registry.connect(cfg("mock")).await.unwrap();

    // A stale reconnect candidate fires for a server that is now Connected
    // (a concurrent connect won the race). Under the fix it aborts on the
    // Connected guard instead of overwriting the live connection with
    // `Reconnecting` — which would strand `id1`, unreachable for teardown.
    Arc::clone(&registry).reconnect_one(cfg("mock")).await;

    let conns = registry.connections.read().await;
    match conns.get("mock") {
        Some(McpConnectionState::Connected { connection_id, .. }) => {
            assert_eq!(
                *connection_id, id1,
                "the live connection id must be preserved"
            );
        }
        other => panic!("expected the connection to stay Connected, got {other:?}"),
    }
}

#[tokio::test]
async fn reconnect_one_reconnects_a_disconnected_server() {
    // Regression: the reconnect path still works for a genuinely
    // reconnect-worthy (non-Connected) state.
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    registry.connections.write().await.insert(
        "mock".into(),
        McpConnectionState::Disconnected {
            config: cfg("mock"),
            last_error: Some("boom".into()),
        },
    );

    Arc::clone(&registry).reconnect_one(cfg("mock")).await;

    let conns = registry.connections.read().await;
    match conns.get("mock") {
        Some(McpConnectionState::Connected { .. }) => {}
        other => panic!("expected reconnect to reach Connected, got {other:?}"),
    }
}

#[tokio::test]
async fn inbound_tools_list_changed_is_forwarded_with_connection_generation() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let (connection, peer_tx, _peer_rx) = drivable_connection();
    let connection_id = ConnId::new();
    let mut changes = registry.subscribe_catalog_changes();
    registry.spawn_catalog_change_listener(
        "srv".into(),
        connection_id,
        connection,
        lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
        ServerCapabilitiesDto {
            tools: true,
            resources: false,
            prompts: false,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        None,
    );

    let mut frame = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed",
        "params": {}
    }))
    .unwrap();
    frame.push(b'\n');
    peer_tx.send(Bytes::from(frame)).await.unwrap();

    let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("catalog notification within timeout")
        .expect("catalog sender remains live");
    assert_eq!(
        change,
        McpCatalogChanged {
            server_name: "srv".into(),
            connection_id,
            retired_connection_id: None,
            kind: McpCatalogKind::Tools,
            telemetry_cause: Some("notification"),
        }
    );
}

#[tokio::test]
async fn inbound_resources_list_changed_is_forwarded() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let (connection, peer_tx, _peer_rx) = drivable_connection();
    let connection_id = ConnId::new();
    let mut changes = registry.subscribe_catalog_changes();
    registry.spawn_catalog_change_listener(
        "srv".into(),
        connection_id,
        connection,
        lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
        ServerCapabilitiesDto {
            tools: false,
            resources: true,
            prompts: false,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        None,
    );

    let mut frame = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/resources/list_changed",
        "params": {}
    }))
    .unwrap();
    frame.push(b'\n');
    peer_tx.send(Bytes::from(frame)).await.unwrap();

    let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("catalog notification within timeout")
        .expect("catalog sender remains live");
    assert_eq!(
        change,
        McpCatalogChanged {
            server_name: "srv".into(),
            connection_id,
            retired_connection_id: None,
            kind: McpCatalogKind::Resources,
            telemetry_cause: Some("notification"),
        }
    );
}

#[tokio::test]
async fn lagged_catalog_listener_recovers_all_supported_catalogs_for_current_generation() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    let pause = Arc::new(Notify::new());

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
    let (connection, peer_tx, mut peer_rx) = drivable_connection();
    let connection_id = ConnId::new();
    set_catalog_change_listener_pause_for_test(connection_id, Some(pause.clone()));
    let client = Arc::new(
        McpClient::new(
            "srv",
            std::path::PathBuf::from("/tmp/work"),
            connection.clone(),
        )
        .await,
    );
    registry.clients.write().await.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(connection_id),
            client,
        },
    );
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: true,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![McpToolDto {
                input_schema_projection: None,
                definition_projection: None,

                server_name: "srv".into(),
                tool_name: "old".into(),
                description: "old".into(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__old".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            resources: vec![resource("old-resource", "file:///old.md")],
            resource_templates: Vec::new(),
            prompts: vec![prompt("old-prompt")],
            connected_at: SystemTime::now(),
        },
    );
    let mut changes = registry.subscribe_catalog_changes();
    registry.spawn_catalog_change_listener(
        "srv".into(),
        connection_id,
        connection,
        lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
        ServerCapabilitiesDto {
            tools: true,
            resources: true,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        None,
    );

    for _ in 0..300 {
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed",
            "params": {}
        }))
        .unwrap();
        frame.push(b'\n');
        peer_tx.send(Bytes::from(frame)).await.unwrap();
    }
    set_catalog_change_listener_pause_for_test(connection_id, None);
    pause.notify_one();

    let refresh_registry = registry.clone();
    let refresh = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..3 {
            let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
                .await
                .expect("lag recovery change within timeout")
                .expect("catalog change sender remains live");
            assert_eq!(change.connection_id, connection_id);
            assert_eq!(change.telemetry_cause, Some(LISTEN_REOPEN_CAUSE));
            refresh_registry
                .refresh_catalog(&change)
                .await
                .expect("refresh succeeds");
            seen.push(change.kind);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), changes.recv())
                .await
                .is_err(),
            "lag recovery must coalesce buffered notifications into one authoritative refresh set"
        );
        seen
    });

    for _ in 0..3 {
        let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("catalog refresh request within timeout")
            .expect("catalog refresh frame");
        let request: Value = serde_json::from_slice(&request_frame).unwrap();
        let method = request["method"].as_str().expect("request method");
        let result = match method {
            "tools/list" => serde_json::json!({
                "tools": [{
                    "name": "fresh-tool",
                    "description": "fresh",
                    "inputSchema": {"type": "object"}
                }]
            }),
            "prompts/list" => serde_json::json!({
                "prompts": [{
                    "name": "fresh-prompt",
                    "description": "fresh",
                    "arguments": []
                }]
            }),
            "resources/list" => serde_json::json!({
                "resources": [{
                    "uri": "file:///fresh.md",
                    "name": "fresh-resource"
                }]
            }),
            other => panic!("unexpected lag recovery request {other}"),
        };
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": result
        }))
        .unwrap();
        response.push(b'\n');
        peer_tx.send(Bytes::from(response)).await.unwrap();
    }

    let mut seen: Vec<&'static str> = refresh
        .await
        .expect("lag refresh join")
        .into_iter()
        .map(|kind| match kind {
            McpCatalogKind::Tools => "tools",
            McpCatalogKind::Prompts => "prompts",
            McpCatalogKind::Resources => "resources",
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, vec!["prompts", "resources", "tools"]);
    let conns = registry.connections.read().await;
    let Some(McpConnectionState::Connected {
        tools,
        prompts,
        resources,
        ..
    }) = conns.get("srv")
    else {
        panic!("server must remain connected");
    };
    assert_eq!(tools[0].tool_name, "fresh-tool");
    assert_eq!(prompts[0].name, "fresh-prompt");
    assert_eq!(resources[0].name, "fresh-resource");
}

#[tokio::test]
async fn lagged_catalog_listener_skips_recovery_for_a_replaced_generation() {
    let pause = Arc::new(Notify::new());

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let (connection, peer_tx, _peer_rx) = drivable_connection();
    let old_connection_id = ConnId::new();
    set_catalog_change_listener_pause_for_test(old_connection_id, Some(pause.clone()));
    let new_connection_id = ConnId::new();
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id: old_connection_id,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: true,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
            connected_at: SystemTime::now(),
        },
    );
    let mut changes = registry.subscribe_catalog_changes();
    registry.spawn_catalog_change_listener(
        "srv".into(),
        old_connection_id,
        connection,
        lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
        ServerCapabilitiesDto {
            tools: true,
            resources: true,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        None,
    );

    for _ in 0..300 {
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed",
            "params": {}
        }))
        .unwrap();
        frame.push(b'\n');
        peer_tx.send(Bytes::from(frame)).await.unwrap();
    }
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id: new_connection_id,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: true,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
            connected_at: SystemTime::now(),
        },
    );
    set_catalog_change_listener_pause_for_test(old_connection_id, None);
    pause.notify_one();

    assert!(
        tokio::time::timeout(Duration::from_millis(200), changes.recv())
            .await
            .is_err(),
        "lag recovery must not publish authoritative refreshes for a replaced generation"
    );
}

#[tokio::test]
async fn modern_listen_request_is_filtered_and_ack_opens_from_zero() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let (connection, peer_tx, mut peer_rx) = drivable_connection();
    let connection_id = ConnId::new();
    // Live installation publishes this generation-bound capability authority
    // before starting its listener.
    let client = Arc::new(
        McpClient::new(
            "srv",
            std::path::PathBuf::from("/tmp/work"),
            connection.clone(),
        )
        .await
        .with_negotiated_protocol(modern_negotiated())
        .with_elicitation_capabilities(lingxi_core::host::McpElicitationCapabilities {
            legacy: lingxi_core::host::McpElicitationMode::Bare,
            modern: lingxi_core::host::McpElicitationMode::FormAndUrl,
        }),
    );
    registry.clients.write().await.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(connection_id),
            client,
        },
    );
    let mut changes = registry.subscribe_catalog_changes();
    registry.spawn_catalog_change_listener(
        "srv".into(),
        connection_id,
        connection,
        modern_negotiated(),
        caps(true, true, false),
        Some(ModernListenOpenTelemetry {
            outcome: telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero,
            attempts: 0,
            trigger: telemetry::tengu::mcp::ListenReopenTrigger::Connect,
        }),
    );

    let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
        .await
        .expect("listen request within timeout")
        .expect("listen request frame");
    let request: Value = serde_json::from_slice(&request_frame).unwrap();
    assert_eq!(request["method"], serde_json::json!("subscriptions/listen"));
    assert_eq!(
        request["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"],
        serde_json::json!({"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}),
        "modern listener retains its frozen Full capability despite Bare initialize"
    );
    assert_eq!(
        request["params"]["notifications"],
        serde_json::json!({
            "toolsListChanged": true,
            "resourcesListChanged": true,
        })
    );

    let mut ack = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": request["id"].clone(),
            }
        }
    }))
    .unwrap();
    ack.push(b'\n');
    peer_tx.send(Bytes::from(ack)).await.unwrap();

    let mut tool_change = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": request["id"].clone(),
            }
        }
    }))
    .unwrap();
    tool_change.push(b'\n');
    peer_tx.send(Bytes::from(tool_change)).await.unwrap();

    let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("matching modern list_changed")
        .expect("catalog sender remains live");
    assert_eq!(change.connection_id, connection_id);
    assert_eq!(change.kind, McpCatalogKind::Tools);
    assert_eq!(change.telemetry_cause, Some("notification"));

    let events = take_test_telemetry_events();
    assert!(events.iter().any(|event| {
        event.name == telemetry::tengu::mcp::LISTEN_REOPEN
            && event.payload.get("outcome") == Some(&serde_json::json!("opened_from_zero"))
            && event.payload.get("attempts") == Some(&serde_json::json!(0))
            && event.payload.get("trigger") == Some(&serde_json::json!("connect"))
    }));
}

#[tokio::test]
async fn modern_catalog_listener_does_not_borrow_replaced_generation_capability_authority() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let (connection, _peer_tx, mut peer_rx) = drivable_connection();
    let stale_connection_id = ConnId::new();
    let current_connection_id = ConnId::new();
    let client = Arc::new(
        McpClient::new(
            "srv",
            std::path::PathBuf::from("/tmp/work"),
            connection.clone(),
        )
        .await
        .with_negotiated_protocol(modern_negotiated()),
    );
    registry.clients.write().await.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(current_connection_id),
            client,
        },
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        registry.run_modern_catalog_change_listener(
            "srv".into(),
            stale_connection_id,
            connection.clone(),
            connection.notifications(),
            caps(true, true, false),
            modern_negotiated(),
            ModernListenOpenTelemetry {
                outcome: telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero,
                attempts: 0,
                trigger: telemetry::tengu::mcp::ListenReopenTrigger::Connect,
            },
        ),
    )
    .await
    .expect("a replaced-generation listener must exit before opening a subscription");
    assert!(
        matches!(
            peer_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "a stale listener must not send using another generation's capability authority"
    );
    connection.close();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn modern_graceful_close_reopens_after_extra_delay_and_publishes_refresh_snapshot() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mut mock = BridgeMock::with_modern_drivable_calls(&["fresh-tool"]);
    mock.resources_capability.store(true, Ordering::SeqCst);
    mock.prompts_capability.store(true, Ordering::SeqCst);
    mock.resources = vec![resource("fresh-resource", "file:///fresh.md")];
    mock.prompts = vec![prompt("fresh-prompt")];
    let mock = Arc::new(mock);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    let first_connection_id = registry.connect(cfg("srv")).await.unwrap();
    let (first_tx, mut first_rx) = mock
        .take_tool_call_peer(first_connection_id)
        .expect("first listen peer");

    let first_listen = first_rx.recv().await.expect("first listen request");
    let first_request: Value = serde_json::from_slice(&first_listen).unwrap();
    assert_eq!(
        first_request["method"],
        serde_json::json!("subscriptions/listen")
    );
    let mut first_ack = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": first_request["id"].clone(),
            }
        }
    }))
    .unwrap();
    first_ack.push(b'\n');
    first_tx.send(Bytes::from(first_ack)).await.unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let mut changes = registry.subscribe_catalog_changes();
    let mut graceful = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": first_request["id"].clone(),
        "result": { "resultType": "complete" }
    }))
    .unwrap();
    graceful.push(b'\n');
    first_tx.send(Bytes::from(graceful)).await.unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let epsilon = Duration::from_millis(1);
    tokio::time::advance(Duration::from_secs(6) - epsilon).await;
    tokio::task::yield_now().await;
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);

    tokio::time::advance(epsilon).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);

    let second_connection_id = {
        let conns = registry.connections.read().await;
        match conns.get("srv") {
            Some(McpConnectionState::Connected { connection_id, .. }) => *connection_id,
            other => panic!("expected reopened connected state, got {other:?}"),
        }
    };
    assert_ne!(second_connection_id, first_connection_id);

    let (second_tx, mut second_rx) = mock
        .take_tool_call_peer(second_connection_id)
        .expect("second listen peer");
    let second_listen = second_rx.recv().await.expect("second listen request");
    let second_request: Value = serde_json::from_slice(&second_listen).unwrap();
    assert_eq!(
        second_request["method"],
        serde_json::json!("subscriptions/listen")
    );
    let mut second_ack = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": second_request["id"].clone(),
            }
        }
    }))
    .unwrap();
    second_ack.push(b'\n');
    second_tx.send(Bytes::from(second_ack)).await.unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let mut seen = Vec::new();
    for _ in 0..3 {
        let change = tokio::time::timeout(Duration::from_secs(1), changes.recv())
            .await
            .expect("reopen snapshot change")
            .expect("catalog changes remain live");
        seen.push(change);
    }
    assert!(seen.iter().any(|change| {
        change.kind == McpCatalogKind::Tools
            && change.connection_id == second_connection_id
            && change.retired_connection_id == Some(first_connection_id)
            && change.telemetry_cause == Some(LISTEN_REOPEN_CAUSE)
    }));
    assert!(seen.iter().any(|change| {
        change.kind == McpCatalogKind::Prompts
            && change.connection_id == second_connection_id
            && change.telemetry_cause == Some(LISTEN_REOPEN_CAUSE)
    }));
    assert!(seen.iter().any(|change| {
        change.kind == McpCatalogKind::Resources
            && change.connection_id == second_connection_id
            && change.telemetry_cause == Some(LISTEN_REOPEN_CAUSE)
    }));

    let events = take_test_telemetry_events();
    assert!(events.iter().any(|event| {
        event.name == telemetry::tengu::mcp::LISTEN_REOPEN
            && event.payload.get("outcome") == Some(&serde_json::json!("opened_from_zero"))
            && event.payload.get("trigger") == Some(&serde_json::json!("connect"))
    }));
    assert!(events.iter().any(|event| {
        event.name == telemetry::tengu::mcp::LISTEN_REOPEN
            && event.payload.get("outcome") == Some(&serde_json::json!("reopened"))
            && event.payload.get("attempts") == Some(&serde_json::json!(1))
            && event.payload.get("trigger") == Some(&serde_json::json!("graceful"))
    }));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn modern_listen_start_send_failure_reopens_on_remote_path() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mock = BridgeMock::with_modern_drivable_calls(&["fresh-tool"]);
    mock.listen_writer_failures_remaining
        .store(1, Ordering::SeqCst);
    let mock = Arc::new(mock);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );

    let first_connection_id = registry.connect(cfg("srv")).await.unwrap();
    assert!(
        mock.take_tool_call_peer(first_connection_id).is_none(),
        "the first generation's listen writer should be closed before any peer can observe it"
    );
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);

    let second_connection_id = {
        let conns = registry.connections.read().await;
        match conns.get("srv") {
            Some(McpConnectionState::Connected { connection_id, .. }) => *connection_id,
            other => panic!("expected reopened connected state, got {other:?}"),
        }
    };
    assert_ne!(second_connection_id, first_connection_id);

    let (second_tx, mut second_rx) = mock
        .take_tool_call_peer(second_connection_id)
        .expect("second listen peer");
    let second_listen = second_rx.recv().await.expect("second listen request");
    let second_request: Value = serde_json::from_slice(&second_listen).unwrap();
    assert_eq!(
        second_request["method"],
        serde_json::json!("subscriptions/listen")
    );
    let mut second_ack = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": second_request["id"].clone(),
            }
        }
    }))
    .unwrap();
    second_ack.push(b'\n');
    second_tx.send(Bytes::from(second_ack)).await.unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let events = take_test_telemetry_events();
    assert!(events.iter().any(|event| {
        event.name == telemetry::tengu::mcp::LISTEN_REOPEN
            && event.payload.get("outcome") == Some(&serde_json::json!("reopened"))
            && event.payload.get("attempts") == Some(&serde_json::json!(1))
            && event.payload.get("trigger") == Some(&serde_json::json!("remote"))
    }));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn modern_listen_start_send_failure_marks_generation_disconnected_after_retry_budget() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let mock = BridgeMock::with_modern_drivable_calls(&["fresh-tool"]);
    mock.listen_writer_failures_remaining
        .store(1, Ordering::SeqCst);
    let mock = Arc::new(mock);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );

    let first_connection_id = registry.connect(cfg("srv")).await.unwrap();
    assert!(
        mock.take_tool_call_peer(first_connection_id).is_none(),
        "the first generation's listen writer should be closed before any peer can observe it"
    );
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    mock.connect_failures_remaining.store(3, Ordering::SeqCst);

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(4)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 4);

    let conns = registry.connections.read().await;
    match conns.get("srv") {
        Some(McpConnectionState::Disconnected {
            last_error: Some(error),
            ..
        }) => {
            assert!(
                error.contains("bridge mock forced connect failure"),
                "unexpected terminal error: {error}"
            );
        }
        other => panic!("expected disconnected terminal state, got {other:?}"),
    }
    drop(conns);

    let events = take_test_telemetry_events();
    assert!(events.iter().any(|event| {
        event.name == telemetry::tengu::mcp::LISTEN_REOPEN
            && event.payload.get("outcome") == Some(&serde_json::json!("gave_up"))
            && event.payload.get("attempts") == Some(&serde_json::json!(3))
            && event.payload.get("trigger") == Some(&serde_json::json!("remote"))
    }));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn modern_listener_budget_exhaustion_parks_and_cancellation_gives_up() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    set_listener_reopen_park_jitter_for_test(Some(1.0));

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let connection_id = ConnId::new();
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id,
            capabilities: caps(true, false, false),
            negotiated: modern_negotiated(),
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
            connected_at: SystemTime::now(),
        },
    );
    registry.listener_reopen_state.write().await.insert(
        "srv".into(),
        ListenerReopenState {
            delay_index: 0,
            opened_at: Some(tokio::time::Instant::now()),
            reopened_at: vec![tokio::time::Instant::now(); LISTENER_REOPEN_MAX_ATTEMPTS_PER_WINDOW],
        },
    );

    let task = tokio::spawn({
        let registry = registry.clone_for_background();
        async move {
            registry
                .handle_modern_catalog_listener_end(
                    "srv",
                    connection_id,
                    telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                )
                .await;
        }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    registry.connections.write().await.remove("srv");
    tokio::time::advance(LISTENER_REOPEN_PARK_POLL).await;
    task.await.expect("budget exhaustion task joins");
    set_listener_reopen_park_jitter_for_test(None);

    let events: Vec<_> = take_test_telemetry_events()
        .into_iter()
        .filter(|event| event.name == telemetry::tengu::mcp::LISTEN_REOPEN)
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|event| event.payload.get("outcome").cloned())
            .collect::<Vec<_>>(),
        vec![
            Some(serde_json::json!("budget_exhausted")),
            Some(serde_json::json!("parked")),
            Some(serde_json::json!("gave_up")),
        ]
    );
    assert!(events
        .iter()
        .all(|event| { event.payload.get("trigger") == Some(&serde_json::json!("remote")) }));
}

#[tokio::test]
async fn registry_notification_subscription_uses_live_client_connection() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let (connection, peer_tx, _peer_rx) = drivable_connection();
    let client =
        Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), connection).await);
    registry.register_client("srv", client).await;

    let mut notifications = registry
        .subscribe_notifications("srv")
        .await
        .expect("a live client must expose its notification stream");
    let mut frame = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/resources/list_changed",
        "params": {"cursor": "next"}
    }))
    .unwrap();
    frame.push(b'\n');
    peer_tx.send(Bytes::from(frame)).await.unwrap();

    let notification = tokio::time::timeout(Duration::from_secs(2), notifications.next())
        .await
        .expect("registry subscription must receive a server push")
        .expect("the live connection must remain open");
    assert_eq!(notification.method, "notifications/resources/list_changed");
    assert_eq!(notification.params, serde_json::json!({"cursor": "next"}));
}

#[tokio::test]
async fn refresh_tools_catalog_replaces_connected_snapshot_after_success() {
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
    let (connection, peer_tx, mut peer_rx) = drivable_connection();
    let client =
        Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), connection).await);
    let connection_id = ConnId::new();
    registry.clients.write().await.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(connection_id),
            client,
        },
    );
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: BridgeMock::new(&["old"]).tools,
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
            connected_at: SystemTime::now(),
        },
    );

    let change = McpCatalogChanged {
        server_name: "srv".into(),
        connection_id,
        retired_connection_id: None,
        kind: McpCatalogKind::Tools,
        telemetry_cause: None,
    };
    let refresh_registry = registry.clone();
    let refresh = tokio::spawn(async move { refresh_registry.refresh_catalog(&change).await });
    let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
        .await
        .expect("tools/list request within timeout")
        .expect("tools/list frame");
    let request: Value = serde_json::from_slice(&request_frame).unwrap();
    assert_eq!(request["method"], "tools/list");
    let mut response = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": request["id"].clone(),
        "result": {
            "tools": [{
                "name": "new tool",
                "description": "fresh",
                "inputSchema": {"type": "object"}
            }]
        }
    }))
    .unwrap();
    response.push(b'\n');
    peer_tx.send(Bytes::from(response)).await.unwrap();

    assert_eq!(
        refresh.await.unwrap().unwrap(),
        Some(connection_id),
        "current connection generation must be refreshed"
    );
    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("srv").unwrap() else {
        panic!("server must remain connected")
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool_name, "new tool");
    assert_eq!(tools[0].full_name, "mcp__srv__new_tool");
}

#[tokio::test]
async fn get_prompt_rejects_generation_swapped_client_after_validation() {
    let registry = Arc::new(McpRegistry::new(Arc::new(BridgeMock::new(&[]))));
    let (conn_a, mut peer_a) = observable_connection();
    let (conn_b, mut peer_b) = observable_connection();
    let client_a =
        Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn_a).await);
    let client_b =
        Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn_b).await);
    let old_id = ConnId::new();
    let new_id = ConnId::new();

    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id: old_id,
            capabilities: ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: vec![McpPromptDto {
                name: "draft".into(),
                description: None,
                arguments: Vec::new(),
            }],
            connected_at: SystemTime::now(),
        },
    );
    let mut clients = registry.clients.write().await;
    clients.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(old_id),
            client: client_a,
        },
    );

    let mut get_prompt = std::pin::pin!(registry.get_prompt(
        old_id,
        "draft",
        serde_json::json!({ "topic": "release" })
    ));
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(get_prompt.as_mut().poll(&mut cx), Poll::Pending));

    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id: new_id,
            capabilities: ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: vec![McpPromptDto {
                name: "draft".into(),
                description: None,
                arguments: Vec::new(),
            }],
            connected_at: SystemTime::now(),
        },
    );
    clients.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(new_id),
            client: client_b,
        },
    );
    drop(clients);

    let error = get_prompt.await.unwrap_err();
    assert!(error.to_string().contains(&format!(
        "MCP prompt connection {old_id} is no longer active"
    )));
    assert!(
        peer_a.try_recv().is_err(),
        "the original generation must not receive prompts/get after replacement",
    );
    assert!(
        peer_b.try_recv().is_err(),
        "the new generation must not receive a stale prompts/get request",
    );
}

#[tokio::test]
async fn cached_prompt_rejects_generation_swapped_after_lazy_dial_upgrade() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry_with_catalog(
        &store,
        &cache_key,
        0,
        ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![],
        vec![],
        vec![McpPromptDto {
            name: "draft".into(),
            description: Some("draft prompt".into()),
            arguments: Vec::new(),
        }],
    );

    let prompt_mock = Arc::new(PromptBridgeMock::new());
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            prompt_mock.clone() as Arc<dyn McpTransport>,
            prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );
    let cached_id = registry
        .connect(cfg)
        .await
        .expect("fresh cache hit connect");
    let live_id = registry
        .ensure_dialed_from_cache("srv")
        .await
        .expect("prompts path lazy dial");
    assert_ne!(
        live_id, cached_id,
        "live generation replaces the cached one"
    );

    let (conn_b, mut peer_b) = observable_connection();
    let client_b =
        Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn_b).await);
    let new_id = ConnId::new();
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: http_cfg("srv", "https://mcp.example.com/v1"),
            connection_id: new_id,
            capabilities: ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: vec![McpPromptDto {
                name: "draft".into(),
                description: Some("draft prompt".into()),
                arguments: Vec::new(),
            }],
            connected_at: SystemTime::now(),
        },
    );
    registry.clients.write().await.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(new_id),
            client: client_b,
        },
    );

    let error = registry
        .get_prompt(
            cached_id,
            "draft",
            serde_json::json!({ "topic": "release" }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&format!(
        "MCP prompt connection {cached_id} is no longer active"
    )));
    assert!(
        peer_b.try_recv().is_err(),
        "the swapped L2 generation must not receive a stale prompts/get request",
    );
    drop(env);
}

#[tokio::test]
async fn ensure_connected_client_accepts_raw_normalized_and_exact_scoped_names() {
    let registry = Arc::new(McpRegistry::new(Arc::new(BridgeMock::new(&[]))));
    let (raw_conn, _raw_peer) = observable_connection();
    let raw_client = Arc::new(
        McpClient::new("my.server", std::path::PathBuf::from("/tmp/work"), raw_conn).await,
    );
    registry
        .register_client("my.server", raw_client.clone())
        .await;

    let (claude_conn, _claude_peer) = observable_connection();
    let claude_client = Arc::new(
        McpClient::new(
            "claude.ai Linear",
            std::path::PathBuf::from("/tmp/work"),
            claude_conn,
        )
        .await,
    );
    registry
        .register_client("claude.ai Linear", claude_client.clone())
        .await;

    let scoped_key = "__lingxi_agent_scope__deadbeef__docs";
    let (scoped_conn, _scoped_peer) = observable_connection();
    let scoped_client =
        Arc::new(McpClient::new("docs", std::path::PathBuf::from("/tmp/work"), scoped_conn).await);
    registry
        .register_client(scoped_key, scoped_client.clone())
        .await;

    let raw_by_raw = registry
        .ensure_connected_client("my.server")
        .await
        .expect("raw name lookup");
    let raw_by_normalized = registry
        .ensure_connected_client("my_server")
        .await
        .expect("normalized lookup");
    let claude_by_raw = registry
        .ensure_connected_client("claude.ai Linear")
        .await
        .expect("raw spaced name");
    let claude_by_normalized = registry
        .ensure_connected_client(&normalize_name_for_mcp("claude.ai Linear"))
        .await
        .expect("normalized spaced name");
    let scoped_by_exact = registry
        .ensure_connected_client(scoped_key)
        .await
        .expect("exact scoped key");

    assert!(Arc::ptr_eq(&raw_by_raw, &raw_client));
    assert!(Arc::ptr_eq(&raw_by_normalized, &raw_client));
    assert!(Arc::ptr_eq(&claude_by_raw, &claude_client));
    assert!(Arc::ptr_eq(&claude_by_normalized, &claude_client));
    assert!(Arc::ptr_eq(&scoped_by_exact, &scoped_client));
}

#[tokio::test]
async fn connected_prompts_cache_generation_reaches_l1_twice_and_cleans_up_on_disconnect() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry_with_catalog(
        &store,
        &cache_key,
        0,
        ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![],
        vec![],
        vec![McpPromptDto {
            name: "draft".into(),
            description: Some("draft prompt".into()),
            arguments: Vec::new(),
        }],
    );

    let prompt_mock = Arc::new(PromptBridgeMock::new());
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            prompt_mock.clone() as Arc<dyn McpTransport>,
            prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("fresh cache hit connect");
    let prompts = registry.connected_prompts().await;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].0, "srv");
    assert_eq!(prompts[0].1, cached_id);
    assert_eq!(prompts[0].2.name, "draft");
    // An agent-scoped registry key reuses the server's config name but must
    // not advertise its prompt in the global slash-command catalog.
    let agent_state = registry
        .connections
        .read()
        .await
        .get("srv")
        .cloned()
        .unwrap();
    registry
        .connections
        .write()
        .await
        .insert("agent:demo:srv".into(), agent_state);
    assert_eq!(registry.connected_prompts().await.len(), 1);
    registry.connections.write().await.remove("agent:demo:srv");

    let responder = {
        let prompt_mock = prompt_mock.clone();
        tokio::spawn(async move {
            let connection_id = prompt_mock.wait_for_connection_id().await;
            prompt_mock.answer_next_prompt(connection_id).await;
            prompt_mock.answer_next_prompt(connection_id).await;
        })
    };

    let first = tokio::time::timeout(
        Duration::from_secs(2),
        registry.get_prompt(
            cached_id,
            "draft",
            serde_json::json!({ "topic": "release" }),
        ),
    )
    .await
    .expect("first cached prompt returns in time")
    .expect("first cached prompt lazy-dial succeeds");
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        registry.get_prompt(
            cached_id,
            "draft",
            serde_json::json!({ "topic": "release" }),
        ),
    )
    .await
    .expect("second cached prompt returns in time")
    .expect("second cached prompt should keep routing C -> L1");
    responder.await.unwrap();
    registry
        .disconnect("srv")
        .await
        .expect("disconnect removes L1");
    let disconnected = registry
        .get_prompt(
            cached_id,
            "draft",
            serde_json::json!({ "topic": "release" }),
        )
        .await
        .unwrap_err();
    drop(env);

    let expected = serde_json::json!({
        "description": "Draft prompt",
        "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
    });
    assert_eq!(
        prompt_mock.connect_calls.load(Ordering::SeqCst),
        1,
        "repeated cached prompt invocations must reuse the first live generation"
    );
    assert_eq!(first, expected);
    assert_eq!(second, expected);
    assert!(disconnected.to_string().contains(&format!(
        "MCP prompt connection {cached_id} is no longer active"
    )));
    assert!(
        !registry
            .prompt_predecessors
            .read()
            .await
            .contains_key(&cached_id),
        "disconnect must retire the cached prompt predecessor bridge"
    );
}

#[tokio::test]
async fn stale_cached_prompt_command_survives_background_upgrade_before_first_invocation() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry_with_catalog(
        &store,
        &cache_key,
        1_000_000,
        ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![],
        vec![],
        vec![McpPromptDto {
            name: "draft".into(),
            description: Some("draft prompt".into()),
            arguments: Vec::new(),
        }],
    );

    let prompt_mock = Arc::new(PromptBridgeMock::new());
    prompt_mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            prompt_mock.clone() as Arc<dyn McpTransport>,
            prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    let prompts = registry.connected_prompts().await;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].1, cached_id);

    prompt_mock.connect_started.notified().await;
    prompt_mock.block_connect.store(false, Ordering::SeqCst);
    prompt_mock.connect_release.notify_one();
    let live_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match registry.connections.read().await.get("srv") {
                Some(McpConnectionState::Connected { connection_id, .. }) => {
                    break *connection_id;
                }
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("background upgrade publishes L1");

    let responder = {
        let prompt_mock = prompt_mock.clone();
        tokio::spawn(async move {
            let connection_id = prompt_mock.wait_for_connection_id().await;
            assert_eq!(connection_id, live_id);
            prompt_mock.answer_next_prompt(connection_id).await;
        })
    };
    let rendered = tokio::time::timeout(
        Duration::from_secs(2),
        registry.get_prompt(
            cached_id,
            "draft",
            serde_json::json!({ "topic": "release" }),
        ),
    )
    .await
    .expect("cached command returns after background publish")
    .expect("C -> L1 predecessor bridge remains valid");
    responder.await.unwrap();
    drop(env);

    assert_eq!(
        prompt_mock.connect_calls.load(Ordering::SeqCst),
        1,
        "using the cached prompt after a background upgrade must not dial L2"
    );
    assert_eq!(
        rendered,
        serde_json::json!({
            "description": "Draft prompt",
            "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
        })
    );
}

#[tokio::test]
async fn connected_state_is_not_visible_until_cached_prompt_predecessor_publishes() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry_with_catalog(
        &store,
        &cache_key,
        0,
        ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![],
        vec![],
        vec![McpPromptDto {
            name: "draft".into(),
            description: Some("draft prompt".into()),
            arguments: Vec::new(),
        }],
    );

    let prompt_mock = Arc::new(PromptBridgeMock::new());
    prompt_mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            prompt_mock.clone() as Arc<dyn McpTransport>,
            prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );
    let cached_id = registry.connect(cfg).await.expect("fresh cache hit");

    let predecessor_guard = registry.prompt_predecessors.write().await;
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    let mut waiter = std::pin::pin!(waiter);
    prompt_mock.connect_started.notified().await;
    prompt_mock.block_connect.store(false, Ordering::SeqCst);
    prompt_mock.connect_release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiter.as_mut())
            .await
            .is_err(),
        "Connected(L1) must not publish before the cached prompt predecessor map can publish"
    );
    drop(predecessor_guard);

    let live_id = tokio::time::timeout(Duration::from_secs(2), waiter.as_mut())
        .await
        .expect("owner finishes after predecessor unlock")
        .expect("join succeeds")
        .expect("lazy dial succeeds");
    let responder = {
        let prompt_mock = prompt_mock.clone();
        tokio::spawn(async move {
            let connection_id = prompt_mock.wait_for_connection_id().await;
            assert_eq!(connection_id, live_id);
            prompt_mock.answer_next_prompt(connection_id).await;
        })
    };
    let rendered = registry
        .get_prompt(
            cached_id,
            "draft",
            serde_json::json!({ "topic": "release" }),
        )
        .await
        .expect("cached prompt C must be callable immediately once L1 is visible");
    responder.await.unwrap();
    drop(env);

    assert_eq!(
        rendered,
        serde_json::json!({
            "description": "Draft prompt",
            "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
        })
    );
}

#[tokio::test]
async fn connect_skips_client_when_raw_conn_none() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    // `new` wires NO RawConnectionProvider.
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    registry.connect(cfg("mock")).await.unwrap();
    assert!(
        registry.get_client("mock").await.is_none(),
        "no client must be registered without a raw_conn bridge"
    );
}

#[tokio::test]
async fn disconnect_drops_registered_client() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("mock")).await.unwrap();
    assert!(registry.get_client("mock").await.is_some());
    registry.disconnect("mock").await.unwrap();
    assert!(
        registry.get_client("mock").await.is_none(),
        "disconnect must drop the cached client"
    );
}

// ---- Batch 2: FQN rewrite + normalize-match ----------------------------

#[tokio::test]
async fn connect_rewrites_fqn_with_normalized_server() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    // Raw name `my.server` normalizes to `my_server` in the FQN, while the
    // stored key (and `server_name`) stay raw for `/mcp` display.
    registry.connect(cfg("my.server")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("my.server").unwrap() else {
        panic!("expected Connected state");
    };
    assert_eq!(tools[0].full_name, "mcp__my_server__read");
    assert_eq!(tools[0].server_name, "my.server");
    drop(conns);

    // A model-supplied normalized `<server>` token resolves the raw key.
    assert!(registry.get_client("my_server").await.is_some());
    assert!(registry.get_config("my_server").await.is_some());
}

/// §26a — a server whose `initialize` declares the `resources` capability
/// has its `resources/templates/list` fetched at connect time and stashed
/// on the `Connected` state, exactly like `resources`/`tools`/`prompts`.
/// Oracle gates `resources/templates/list` on the SAME capability as
/// `resources/list` (@167690139), not a separate template bit — this is
/// the connect-time "catalog fetch" the audit found entirely absent
/// (registry.rs's fetch was `list_tools`/`list_resources`/`list_prompts`
/// A stdio server must NOT be asked for `resources/templates/list`, even
/// with the `resources` capability present.
///
/// The oracle's live-discovery fetch (@182539595) is
/// `v && JK(G.config) ? Qe(G) : Promise.resolve([])` — the `resources`
/// capability AND cache eligibility. `JK` (@176260324) rejects any
/// transport that is not `http`/`sse` outright, and the cache itself is
/// off unless `MCP_DISCOVERY_CACHE` or the `tengu_mcp_discovery_cache_enable`
/// gate says otherwise (both default off). So with stock settings the
/// oracle issues ZERO of these RPCs, and never any for stdio.
///
/// The assertion is the CALL COUNT, not the stored field: fetching and
/// discarding would leave `resource_templates` empty too, and would still
/// be the extra round trip this pins against.
#[tokio::test]
async fn connect_does_not_issue_the_templates_rpc_when_the_cache_is_ineligible() {
    let mock = Arc::new(BridgeMock::with_resource_templates(vec![
        lingxi_core::host::McpResourceTemplateDto {
            uri_template: "file:///{path}".into(),
            name: "file-template".into(),
            description: Some("A file on disk".into()),
            mime_type: Some("text/plain".into()),
            annotations: None,
            meta: None,
        },
    ]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    // `cfg` builds a stdio spec, which `JK` rejects on transport alone.
    registry.connect(cfg("srv")).await.unwrap();

    assert_eq!(
        mock.templates_calls.load(Ordering::SeqCst),
        0,
        "a stdio server is cache-ineligible, so the oracle never issues \
         resources/templates/list for it — the port must not either"
    );
    let conns = registry.connections.read().await;
    let McpConnectionState::Connected {
        resource_templates, ..
    } = conns.get("srv").unwrap()
    else {
        panic!("expected Connected state");
    };
    assert!(
        resource_templates.is_empty(),
        "no fetch means no templates, got {resource_templates:?}"
    );
}

/// The capability gate: without the `resources` capability the fetch must
/// be SKIPPED entirely (mirrors the existing `resources`/`prompts` gates
/// just above it), not merely returning empty because the mock had none.
#[tokio::test]
async fn connect_skips_resource_templates_fetch_without_resources_capability() {
    // `BridgeMock::new` reports `resources: false` from `initialize`, so
    // even a mock stocked with templates must yield none on connect.
    let mut mock = BridgeMock::new(&[]);
    mock.resource_templates = vec![lingxi_core::host::McpResourceTemplateDto {
        uri_template: "file:///{path}".into(),
        name: "unreachable".into(),
        description: None,
        mime_type: None,
        annotations: None,
        meta: None,
    }];
    let mock = Arc::new(mock);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("srv")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected {
        resource_templates, ..
    } = conns.get("srv").unwrap()
    else {
        panic!("expected Connected state");
    };
    assert!(
        resource_templates.is_empty(),
        "resources capability absent -> templates fetch must be skipped, got {resource_templates:?}"
    );
}

/// §26a — a `resources/templates/list` FAILURE must never fail the
/// connection. Templates are optional in the MCP spec: a server can
/// legally declare `capabilities.resources` (because it registered
/// `resources/list`) and answer `-32601 Method not found` for
/// `resources/templates/list`. Oracle `Qe` (2.1.251 Mach-O @182528544)
/// wraps the whole fetch in `try{...}catch(t){ ...; let r=[]; if(!(t
/// instanceof Er&&t.code===Ir.MethodNotFound)) qt().discoveryFetchErrors
/// .set(r,we(t)); return r }` — EVERY error path returns an empty array,
/// and MethodNotFound is explicitly benign. Joining the fetch to the
/// catalog block with `?` instead disconnected the live transport
/// (registry.rs's `Err` arm) and returned `Err` from `connect`, so a
/// server that connected fine before the fetch existed lost ALL of its
/// A `-32601` on `resources/templates/list` must not fail the connection.
///
/// This drives the ELIGIBLE path on purpose: an http spec with the
/// discovery cache enabled is the only shape for which the oracle issues
/// the RPC at all (`v && JK(G.config)`, @182539595), so it is the only
/// shape under which this failure mode can arise. Gating the fetch made
/// the previous stdio-based version of this test vacuous — the fetch
/// never ran, so `list_resource_templates_fails` had nothing to fail.
///
/// Oracle `Qe` @182528544 wraps the whole fetch in a catch that returns
/// `[]` on EVERY error, so it can never fail a connection.
#[tokio::test]
async fn connect_survives_a_resource_templates_fetch_that_fails() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.resources_capability.store(true, Ordering::SeqCst);
    mock.list_resource_templates_fails
        .store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    let connected = registry
        .connect(http_cfg("srv", "https://mcp.example.com/v1"))
        .await;
    drop(env);

    assert!(
        connected.is_ok(),
        "a -32601 on resources/templates/list must NOT fail the connection, got {:?}",
        connected.err()
    );
    assert_eq!(
        mock.templates_calls.load(Ordering::SeqCst),
        1,
        "the eligible path must actually issue the RPC, or this test proves nothing"
    );
    let conns = registry.connections.read().await;
    let McpConnectionState::Connected {
        tools,
        resource_templates,
        ..
    } = conns.get("srv").unwrap()
    else {
        panic!("expected Connected state");
    };
    assert_eq!(tools.len(), 1, "the server's tools must survive");
    assert!(
        resource_templates.is_empty(),
        "a failed template fetch yields an empty list, not an error"
    );
}

// ── §11 discovery-cache wiring ──────────────────────────────────────

#[test]
fn secret_refusal_extracts_composite_header_values() {
    let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let McpTransportSpec::Http { headers, .. } = &mut cfg.spec else {
        unreachable!()
    };
    headers.insert(
        "Cookie".to_string(),
        "session=super-secret-value; theme=dark".to_string(),
    );

    let candidates = McpRegistry::config_secret_candidates(&cfg);
    assert!(candidates.iter().any(|value| value == "super-secret-value"));
    let serialized = serde_json::json!({"description": "super-secret-value"}).to_string();
    assert!(McpRegistry::discovery_cache_entry_reflects_secret(
        &serialized,
        &candidates
    ));
}

#[test]
fn secret_refusal_detects_percent_encoded_token_values() {
    let candidates = vec!["tok+/=value?".to_string()];
    for reflected in ["tok%2B%2F%3Dvalue%3F", "tok%2b%2f%3dvalue%3f"] {
        let serialized = serde_json::json!({"description": reflected}).to_string();
        assert!(McpRegistry::discovery_cache_entry_reflects_secret(
            &serialized,
            &candidates
        ));
    }
}

#[tokio::test]
async fn secret_refusal_checks_percent_encoded_stored_mcp_tokens() {
    let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let McpTransportSpec::Http { oauth, .. } = &mut cfg.spec else {
        unreachable!()
    };
    *oauth = Some(lingxi_core::host::McpOAuthConfigDto {
        client_id: None,
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    });

    let storage = Arc::new(XaaMemStorage::default());
    let storage_dyn = storage.clone() as Arc<dyn lingxi_core::host::SecureStorage>;
    let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn lingxi_core::host::Clock>;
    let server_key = oauth::server_key(&cfg.name, &cfg.spec);
    oauth::store_tokens(
        &storage_dyn,
        &clock,
        &server_key,
        &oauth::StoredTokens {
            access_token: "access+/=token?".into(),
            refresh_token: Some("refresh+/=token?".into()),
            expires_at_unix: 1,
            client_id: None,
            client_secret: None,
            step_up_scope: None,
        },
    )
    .await
    .expect("store MCP tokens");

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_oauth(OAuthDeps {
        http: GatedXaaHttp::new() as Arc<dyn lingxi_core::host::HttpTransport>,
        clock,
        storage: storage_dyn,
        on_authorization_url: Arc::new(|_| {}),
        xaa_config: None,
    });
    let candidates = registry
        .discovery_cache_secret_candidates_for(&cfg)
        .await
        .expect("secret candidates");

    for reflected in ["access%2B%2F%3Dtoken%3F", "refresh%2B%2F%3Dtoken%3F"] {
        let serialized = serde_json::json!({"description": reflected}).to_string();
        assert!(McpRegistry::discovery_cache_entry_reflects_secret(
            &serialized,
            &candidates
        ));
    }
}

#[tokio::test]
async fn oauth_grant_provenance_writes_same_grant_and_rejects_rotation_for_each_scope() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    for (name, scope, source) in [
        (
            "shared-grant",
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            None,
        ),
        (
            "agent-grant",
            ConfigScope::Agent,
            Some(crate::connection::McpAgentSource::BuiltIn),
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let mut cfg = http_cfg(name, "https://mcp.example.com/v1");
        cfg.scope = scope;
        cfg.metadata.agent_source = source;
        let McpTransportSpec::Http {
            oauth: oauth_config,
            ..
        } = &mut cfg.spec
        else {
            unreachable!()
        };
        *oauth_config = Some(lingxi_core::host::McpOAuthConfigDto {
            client_id: None,
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });

        let storage = Arc::new(XaaMemStorage::default());
        let storage_dyn = storage.clone() as Arc<dyn lingxi_core::host::SecureStorage>;
        let clock =
            Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn lingxi_core::host::Clock>;
        let server_key = oauth::server_key(&cfg.name, &cfg.spec);
        let store_token = |access: &str, refresh: &str| {
            let storage = storage_dyn.clone();
            let clock = clock.clone();
            let server_key = server_key.clone();
            let access = access.to_string();
            let refresh = refresh.to_string();
            async move {
                oauth::store_tokens(
                    &storage,
                    &clock,
                    &server_key,
                    &oauth::StoredTokens {
                        access_token: access,
                        refresh_token: Some(refresh),
                        expires_at_unix: u64::MAX,
                        client_id: None,
                        client_secret: None,
                        step_up_scope: None,
                    },
                )
                .await
                .expect("store test grant");
            }
        };
        store_token("access-a", "refresh-a").await;

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_oauth(OAuthDeps {
            http: GatedXaaHttp::new() as Arc<dyn lingxi_core::host::HttpTransport>,
            clock: clock.clone(),
            storage: storage_dyn.clone(),
            on_authorization_url: Arc::new(|_| {}),
            xaa_config: None,
        })
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let grant = registry
            .current_grant_provenance(&cfg)
            .await
            .expect("current grant")
            .expect("OAuth storage grant");
        let partition = registry
            .discovery_cache_partition_for_grant(
                &cfg,
                crate::protocol_negotiation::NegotiationMode::Legacy,
                Some(&grant),
            )
            .await
            .expect("grant partition");
        let caps = ServerCapabilitiesDto {
            tools: true,
            ..ServerCapabilitiesDto::default()
        };
        let protocol = lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        };
        let tool = |name: &str| McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: cfg.name.clone(),
            tool_name: name.into(),
            description: name.into(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: format!("mcp__{}__{name}", cfg.name),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        };
        registry
            .persist_or_purge_discovery_cache(
                &cfg,
                Some(&partition),
                &caps,
                &[tool("alpha")],
                &[],
                &[],
                &[],
                crate::protocol_negotiation::NegotiationMode::Legacy,
                Some(&grant),
                Some(&protocol),
                None,
            )
            .await;
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        let saved = store.load_partitioned(&cache_key, &partition.partition_key);
        assert!(matches!(
            saved,
            crate::discovery_cache::EntryLookup::Found(_)
        ));

        store_token("access-b", "refresh-b").await;
        registry
            .persist_or_purge_discovery_cache(
                &cfg,
                Some(&partition),
                &caps,
                &[tool("beta")],
                &[],
                &[],
                &[],
                crate::protocol_negotiation::NegotiationMode::Legacy,
                Some(&grant),
                Some(&protocol),
                None,
            )
            .await;
        let saved = store.load_partitioned(&cache_key, &partition.partition_key);
        assert!(matches!(
            saved,
            crate::discovery_cache::EntryLookup::Found(entry)
                if entry.tools.iter().any(|tool| tool.tool_name == "alpha")
                    && entry.tools.iter().all(|tool| tool.tool_name != "beta")
        ));
        let rotated = registry
            .current_grant_provenance(&cfg)
            .await
            .expect("rotated grant")
            .expect("rotated OAuth storage grant");
        let rotated_partition = registry
            .discovery_cache_partition_for_grant(
                &cfg,
                crate::protocol_negotiation::NegotiationMode::Legacy,
                Some(&rotated),
            )
            .await
            .expect("rotated partition");
        assert_ne!(partition.partition_key, rotated_partition.partition_key);
        assert!(matches!(
            store.load_partitioned(&cache_key, &rotated_partition.partition_key),
            crate::discovery_cache::EntryLookup::Absent
        ));
    }
    drop(env);
}

#[tokio::test]
async fn oauth_without_refresh_grant_is_not_partition_or_write_eligible() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = http_cfg("access-only", "https://mcp.example.com/v1");
    let McpTransportSpec::Http {
        oauth: oauth_config,
        ..
    } = &mut cfg.spec
    else {
        unreachable!()
    };
    *oauth_config = Some(lingxi_core::host::McpOAuthConfigDto {
        client_id: None,
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    });
    let storage = Arc::new(XaaMemStorage::default());
    let storage_dyn = storage.clone() as Arc<dyn lingxi_core::host::SecureStorage>;
    let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn lingxi_core::host::Clock>;
    let key = oauth::server_key(&cfg.name, &cfg.spec);
    oauth::store_tokens(
        &storage_dyn,
        &clock,
        &key,
        &oauth::StoredTokens {
            access_token: "access-only".into(),
            refresh_token: None,
            expires_at_unix: u64::MAX,
            client_id: None,
            client_secret: None,
            step_up_scope: None,
        },
    )
    .await
    .expect("store access-only token");
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_oauth(OAuthDeps {
        http: GatedXaaHttp::new() as Arc<dyn lingxi_core::host::HttpTransport>,
        clock,
        storage: storage_dyn,
        on_authorization_url: Arc::new(|_| {}),
        xaa_config: None,
    })
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    assert!(registry
        .current_grant_provenance(&cfg)
        .await
        .expect("current grant")
        .is_none());
    assert!(matches!(
        registry
            .discovery_cache_partition_for(
                &cfg,
                crate::protocol_negotiation::NegotiationMode::Legacy,
            )
            .await,
        Err(crate::discovery_cache::MissReason::NoFingerprint)
    ));
    registry
        .persist_or_purge_discovery_cache(
            &cfg,
            None,
            &ServerCapabilitiesDto::default(),
            &[],
            &[],
            &[],
            &[],
            crate::protocol_negotiation::NegotiationMode::Legacy,
            None,
            None,
            None,
        )
        .await;
    assert!(
        std::fs::read_dir(dir.path())
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "access-only OAuth must never create a discovery-cache partition"
    );
    drop(env);
}

/// A `None` `discovery_cache_store` (every registry not built with
/// [`McpRegistry::with_discovery_cache_store`]) must leave every §11
/// helper a total no-op: no store, no write, no purge, no strike, no
/// telemetry decision. This is the guard that keeps every EXISTING
/// connect test in this file (none of which wires a store) unaffected.
#[tokio::test]
async fn no_store_configured_is_a_total_no_op() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    let result = registry
        .connect(http_cfg("srv", "https://mcp.example.com/v1"))
        .await;
    drop(env);

    assert!(result.is_ok(), "no store must never perturb a connect");
}

/// The write half: a successful live discovery for a cache-ELIGIBLE
/// server (http, feature enabled, no headers helper) must persist the
/// FULL catalog to disk, versioned, with strikes reset to 0. Asserted
/// against the store's own `load`, not the `Connected` state — proving
/// the SEPARATE persistence path actually ran.
#[tokio::test]
async fn connect_persists_a_discovery_cache_entry_for_an_eligible_server() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    registry.connect(cfg).await.unwrap();
    drop(env);

    let entry = match load_test_entry(&store, &cache_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("expected a persisted entry, got {other:?}"),
    };
    assert_eq!(entry.version, crate::discovery_cache::CACHE_SCHEMA_VERSION);
    assert_eq!(entry.cache_key, cache_key);
    assert_eq!(entry.consecutive_refresh_failures, 0);
    assert!(
        entry.server_info.is_none(),
        "the existing metadata-free handshake stays absent"
    );
    assert!(
        entry.capabilities.tools,
        "the mock declares the tools capability"
    );
    assert_eq!(
        entry
            .tools
            .iter()
            .map(|t| t.tool_name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha"],
        "the persisted entry must carry the ACTUAL discovered catalog"
    );
}

#[tokio::test]
async fn discovery_cache_roundtrip_retains_only_server_info_identity() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");
    let dir = tempfile::tempdir().unwrap();
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let config = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&config);
    let mut transport = BridgeMock::new(&["alpha"]);
    let metadata = lingxi_core::host::McpServerMetadataDto {
        server_info: Some(serde_json::json!({
            "name": "calendar-server",
            "version": "1.2.3",
            "title": "LIVE_ONLY title",
            "icons": [{"src": "LIVE_ONLY icon"}],
            "description": "LIVE_ONLY description",
            "instructions": "LIVE_ONLY nested instructions"
        })),
        raw_capabilities: Some(serde_json::json!({"LIVE_ONLY": "capability details"})),
        instructions: Some("LIVE_ONLY server instructions".into()),
        discovery: Some(serde_json::json!({"LIVE_ONLY": "discovery result"})),
    };
    transport.server_metadata = Some(metadata.clone());
    let live_transport = Arc::new(transport);
    let live = McpRegistry::with_raw_conn(
        live_transport.clone() as Arc<dyn McpTransport>,
        live_transport.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    live.connect(config.clone()).await.unwrap();
    assert_eq!(live_transport.connect_calls.load(Ordering::SeqCst), 1);
    assert_eq!(live.server_metadata("srv").await, Some(metadata));

    let entry = match load_test_entry(&store, &cache_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("live discovery did not persist an entry: {other:?}"),
    };
    let identity = serde_json::json!({"name": "calendar-server", "version": "1.2.3"});
    assert_eq!(serde_json::to_value(&entry.server_info).unwrap(), identity);
    assert!(!serde_json::to_string(&entry).unwrap().contains("LIVE_ONLY"));
    // Explicit disconnect purges the cache by design; dropping the old registry
    // simulates the next host startup without removing its persisted discovery.
    drop(live);
    drop(live_transport);

    // A new registry must obtain identity from the persisted cache, not a retained transport.
    let cached_transport = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
    let cached = McpRegistry::with_raw_conn(
        cached_transport.clone() as Arc<dyn McpTransport>,
        cached_transport.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    cached.connect(config).await.unwrap();
    assert!(matches!(
        cached.connections.read().await.get("srv"),
        Some(McpConnectionState::Cached { .. })
    ));
    assert_eq!(cached_transport.connect_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        cached.server_metadata("srv").await,
        Some(lingxi_core::host::McpServerMetadataDto {
            server_info: Some(identity),
            ..Default::default()
        }),
        "cached metadata must expose only name/version, without live capabilities/instructions/discovery"
    );
    assert!(cached.server_instruction_blocks().await.is_empty());
    cached.set_disabled("srv", true).await.unwrap();
    assert!(cached.server_metadata("srv").await.is_none());
    assert_eq!(cached_transport.connect_calls.load(Ordering::SeqCst), 0);
}

/// A gate-ineligible server (stdio: [`crate::discovery_cache::CacheGateReason::Transport`])
/// must never touch the store at all, even with a store wired and the
/// feature enabled.
#[tokio::test]
async fn connect_does_not_persist_for_a_transport_ineligible_server() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    // `cfg` builds a stdio spec — `Transport`-ineligible regardless of
    // the feature flag.
    registry.connect(cfg("srv")).await.unwrap();
    drop(env);

    assert!(
        std::fs::read_dir(dir.path())
            .map(|mut it| it.next().is_none())
            .unwrap_or(true),
        "a stdio (transport-ineligible) connect must create NO cache files at all"
    );
}

/// The purge half: [`McpRegistry::persist_or_purge_discovery_cache`]
/// must remove any EXISTING on-disk entry when the gate reason is
/// [`crate::discovery_cache::CacheGateReason::HeadersHelper`] — the one
/// gate reason besides `OptOut` that
/// [`crate::discovery_cache::CacheGateReason::purges_existing_entry`]
/// flags. Called directly (not through `connect`) because a real
/// `headersHelper` would spawn an actual subprocess —
/// the gate decision itself does not depend on that subprocess ever
/// running, only on `config.spec` carrying `headers_helper: Some(_)`.
#[tokio::test]
async fn persist_or_purge_removes_an_existing_entry_when_the_gate_is_headers_helper() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let McpTransportSpec::Http { headers_helper, .. } = &mut cfg.spec else {
        unreachable!()
    };
    *headers_helper = Some("./helper".to_string());
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);

    // Pre-seed an entry as if it were written before `headersHelper` got
    // configured on this server.
    store_test_entry(
        &store,
        &crate::discovery_cache::DiscoveryCacheEntry::new(
            cache_key.clone(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );
    assert!(matches!(
        load_test_entry(&store, &cache_key),
        crate::discovery_cache::EntryLookup::Found(_)
    ));

    registry
        .persist_or_purge_discovery_cache(
            &cfg,
            None,
            &ServerCapabilitiesDto::default(),
            &[],
            &[],
            &[],
            &[],
            crate::protocol_negotiation::NegotiationMode::Legacy,
            None,
            None,
            None,
        )
        .await;
    drop(env);

    assert_eq!(
        load_test_entry(&store, &cache_key),
        crate::discovery_cache::EntryLookup::Absent,
        "a headersHelper-gated server must have its stale entry purged"
    );
}

#[tokio::test]
async fn provenance_gates_skip_cache_read_and_purge() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let mut cli_owned = http_cfg("cli-owned", "https://mcp.example.com/v1");
    cli_owned.metadata.cli_owned = true;
    let env_placeholder = http_cfg("env-placeholder", "https://${MCP_HOST}/v1");
    let mut ambient_credential = http_cfg("ambient-credential", "https://mcp.example.com/v1");
    ambient_credential.metadata.ambient_credential = true;
    let mut agent_without_source = http_cfg("agent-without-source", "https://mcp.example.com/v1");
    agent_without_source.scope = ConfigScope::Agent;
    let scenarios = [
        (cli_owned, crate::discovery_cache::MissReason::CliOwned),
        (
            env_placeholder,
            crate::discovery_cache::MissReason::EnvPlaceholder,
        ),
        (
            ambient_credential,
            crate::discovery_cache::MissReason::AmbientCredential,
        ),
        (
            agent_without_source,
            crate::discovery_cache::MissReason::NoFingerprint,
        ),
    ];

    for (config, expected_reason) in scenarios {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cache_key = crate::discovery_cache::logical_cache_key(&config);
        store_test_entry(
            &store,
            &crate::discovery_cache::DiscoveryCacheEntry::new(
                cache_key.clone(),
                1,
                ServerCapabilitiesDto::default(),
                vec![],
                vec![],
                vec![],
                vec![],
            ),
        );
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let consult = registry
            .discovery_cache_decision_for(
                &config,
                crate::protocol_negotiation::NegotiationMode::Legacy,
            )
            .await
            .expect("store is configured");
        assert_eq!(
            consult.decision,
            crate::discovery_cache::Decision::Miss {
                reason: expected_reason
            },
            "provenance gate must short-circuit before an on-disk lookup"
        );
        assert!(
            consult.partition.is_none(),
            "provenance gate must not resolve a cache partition"
        );
        assert!(matches!(
            load_test_entry(&store, &cache_key),
            crate::discovery_cache::EntryLookup::Found(_)
        ));
    }
    drop(env);
}

/// A real parsed/runtime `discoveryCache:false` value must purge the
/// server's existing cache family, dial live, and decline write-through.
#[tokio::test]
async fn discovery_cache_false_purges_then_dials_without_rewriting() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
    cfg.discovery_cache = Some(false);
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    store_test_entry(
        &store,
        &crate::discovery_cache::DiscoveryCacheEntry::new(
            cache_key.clone(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry.connect(cfg).await.expect("live opt-out connect");
    drop(env);

    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        load_test_entry(&store, &cache_key),
        crate::discovery_cache::EntryLookup::Absent,
        "opt-out must purge the old partition and must not write a new one"
    );
}

/// A cached dial strikes only when the connection fails. A catalog failure after successful
/// initialize must not strike an existing cache entry.
#[tokio::test]
async fn an_ordinary_partial_connect_does_not_strike_an_existing_entry() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    store_test_entry(
        &store,
        &crate::discovery_cache::DiscoveryCacheEntry::new(
            cache_key.clone(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        ),
    );

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let connection_id = registry
        .connect(cfg)
        .await
        .expect("transport initialize succeeds despite tools/list failure");
    drop(env);

    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Connected { connection_id: current, tools, .. })
            if *current == connection_id && tools.is_empty()
    ));
    let entry = match load_test_entry(&store, &cache_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("expected the seeded entry to survive, got {other:?}"),
    };
    assert_eq!(
        entry.consecutive_refresh_failures, 0,
        "ordinary partial connect must not record a stale-refresh strike"
    );
}

#[test]
fn stale_refresh_strikes_only_the_partition_that_served_the_hit() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _env = DiscoveryCacheEnvGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let logical_key = crate::discovery_cache::logical_cache_key(&cfg);
    let old_partition = DiscoveryCachePartition {
        logical_key: logical_key.clone(),
        partition_key: crate::discovery_cache::partition_key(
            &logical_key,
            &crate::discovery_cache::fingerprint("grant:old"),
        ),
        expected_era: "legacy",
        negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
    };
    let new_partition = DiscoveryCachePartition {
        logical_key: logical_key.clone(),
        partition_key: crate::discovery_cache::partition_key(
            &logical_key,
            &crate::discovery_cache::fingerprint("grant:new"),
        ),
        expected_era: "legacy",
        negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
    };
    let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
        logical_key.clone(),
        1,
        ServerCapabilitiesDto::default(),
        vec![],
        vec![],
        vec![],
        vec![],
    );
    store
        .store_partitioned(&entry, &old_partition.partition_key)
        .expect("seed old partition");
    store
        .store_partitioned(&entry, &new_partition.partition_key)
        .expect("seed rotated partition");

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry.record_discovery_cache_refresh_failure(&cfg, &old_partition);

    assert_eq!(
        store.load_partitioned(&logical_key, &old_partition.partition_key),
        crate::discovery_cache::EntryLookup::Absent
    );
    let new = match store.load_partitioned(&logical_key, &new_partition.partition_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("expected rotated partition, got {other:?}"),
    };
    assert_eq!(
        new.consecutive_refresh_failures, 0,
        "a refresh-token rotation must not move the strike to the new partition"
    );
}

#[tokio::test]
async fn stale_refresh_era_change_purges_hit_partition_without_replacement_or_strike() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let logical_key = crate::discovery_cache::logical_cache_key(&cfg);
    let partition = DiscoveryCachePartition {
        logical_key: logical_key.clone(),
        partition_key: crate::discovery_cache::partition_key(
            &logical_key,
            &crate::discovery_cache::fingerprint("grant:none"),
        ),
        expected_era: "legacy",
        negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
    };
    let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
        logical_key.clone(),
        1,
        ServerCapabilitiesDto::default(),
        vec![],
        vec![],
        vec![],
        vec![],
    );
    store
        .store_partitioned(&entry, &partition.partition_key)
        .expect("seed stale partition");
    let other_partition_key = crate::discovery_cache::partition_key(
        &logical_key,
        &crate::discovery_cache::fingerprint("grant:other"),
    );
    store
        .store_partitioned(&entry, &other_partition_key)
        .expect("seed other partition");

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    let cached_connection_id = registry
        .serve_discovery_cache_hit(&cfg, "srv", entry, 1_000_000, false, None)
        .await
        .expect("cache hit");
    let slot = Arc::new(LazyUpgradeSlot::new(
        "srv".into(),
        cached_connection_id,
        cfg.clone(),
        Some(partition.clone()),
        Some("legacy".into()),
        crate::protocol_negotiation::NegotiationMode::Legacy,
        LazyUpgradeMode::Background,
    ));
    registry
        .lazy_upgrade_slots
        .write()
        .await
        .insert("srv".into(), slot.clone());

    let discovery = LiveDiscovery {
        connection_id: McpConnectionId::new(),
        connection_duration_ms: 1,
        grant_provenance: Some(GrantProvenance::unbound()),
        negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
        negotiated: lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Modern,
            version: "2026-07-28".into(),
        },
        capabilities: ServerCapabilitiesDto::default(),
        tools: vec![],
        resources: vec![],
        resource_templates: vec![],
        prompts: vec![],
        catalog_failures: CatalogFetchFailures::default(),
        discovery_cache_partition: Some(partition.clone()),
        client: None,
        listener_connection: None,
    };
    assert!(matches!(
        registry
            .install_lazy_upgrade_live_discovery_if_current("srv", &slot, discovery)
            .await,
        BackgroundInstallOutcome::Rejected(_)
    ));
    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Cached { connection_id, .. })
            if *connection_id == cached_connection_id
    ));
    assert_eq!(
        store.load_partitioned(&logical_key, &partition.partition_key),
        crate::discovery_cache::EntryLookup::Absent,
        "an era change retires only the partition that served the stale hit"
    );
    assert!(matches!(
        store.load_partitioned(&logical_key, &other_partition_key),
        crate::discovery_cache::EntryLookup::Found(entry)
            if entry.consecutive_refresh_failures == 0
    ));
}

#[tokio::test]
async fn stale_refresh_expected_mode_change_purges_without_publish_strike_or_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let logical_key = crate::discovery_cache::logical_cache_key(&cfg);
    let partition = DiscoveryCachePartition {
        logical_key: logical_key.clone(),
        partition_key: crate::discovery_cache::partition_key(
            &logical_key,
            &crate::discovery_cache::fingerprint("grant:none"),
        ),
        expected_era: "legacy",
        negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
    };
    let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
        logical_key.clone(),
        1,
        ServerCapabilitiesDto::default(),
        vec![],
        vec![],
        vec![],
        vec![],
    );
    store
        .store_partitioned(&entry, &partition.partition_key)
        .expect("seed stale partition");

    let mock = Arc::new(BridgeMock::new(&[]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    let cached_connection_id = registry
        .serve_discovery_cache_hit(&cfg, "srv", entry, 1_000_000, false, None)
        .await
        .expect("cache hit");
    let slot = Arc::new(LazyUpgradeSlot::new(
        "srv".into(),
        cached_connection_id,
        cfg.clone(),
        Some(partition.clone()),
        Some("legacy".into()),
        crate::protocol_negotiation::NegotiationMode::Legacy,
        LazyUpgradeMode::Background,
    ));
    registry
        .lazy_upgrade_slots
        .write()
        .await
        .insert("srv".into(), slot.clone());

    let discovery = LiveDiscovery {
        connection_id: McpConnectionId::new(),
        connection_duration_ms: 1,
        grant_provenance: Some(GrantProvenance::unbound()),
        // The live handshake actually stayed legacy, but its immutable
        // resolver mode changed. That is enough to reject the stale hit.
        negotiation_mode: crate::protocol_negotiation::NegotiationMode::Auto {
            probe_timeout_ms: 1_000,
        },
        negotiated: lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
        capabilities: ServerCapabilitiesDto::default(),
        tools: vec![],
        resources: vec![],
        resource_templates: vec![],
        prompts: vec![],
        catalog_failures: CatalogFetchFailures::default(),
        discovery_cache_partition: Some(partition.clone()),
        client: None,
        listener_connection: None,
    };
    assert!(matches!(
        registry
            .install_lazy_upgrade_live_discovery_if_current("srv", &slot, discovery)
            .await,
        BackgroundInstallOutcome::Rejected(_)
    ));
    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Cached { connection_id, .. })
            if *connection_id == cached_connection_id
    ));
    assert_eq!(
        store.load_partitioned(&logical_key, &partition.partition_key),
        crate::discovery_cache::EntryLookup::Absent,
        "expected mode drift purges the partition that served the stale hit"
    );
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
}

/// A server with no prior cache entry still persists the connected
/// generation when transport initialize succeeds and only `tools/list`
/// fails. The stored entry must remain strike-free and carry the empty
/// degraded tools slice.
#[tokio::test]
async fn a_partial_connect_with_no_existing_entry_persists_a_strike_free_empty_catalog() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let connection_id = registry.connect(cfg.clone()).await;
    let negotiation_mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
        &cfg.spec,
        cfg.metadata.transport.as_deref(),
        mcp_connection_timeout().as_millis() as u64,
    );
    let partition = registry
        .discovery_cache_partition_for(&cfg, negotiation_mode)
        .await
        .expect("eligible config gets a partition");
    drop(env);

    let connection_id = connection_id.expect("partial connect still succeeds");
    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Connected { connection_id: current, tools, .. })
            if *current == connection_id && tools.is_empty()
    ));
    let entry = match store.load_partitioned(&cache_key, &partition.partition_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("expected a newly persisted entry, got {other:?}"),
    };
    assert_eq!(entry.consecutive_refresh_failures, 0);
    assert!(
        entry.tools.is_empty(),
        "failed tools/list persists the degraded empty tools slice on first connect"
    );
}

// ── §11 Stage 2: serve discovery-cache hits without dialing, lazy dial ──

/// Seed a discovery-cache entry for `srv`, saved `age_ms` in the past
/// (relative to "now"), for the Stage 2/3 cache tests below. `age_ms`
/// alone decides Fresh (< 900s default TTL) vs Stale (>= TTL,
/// < 14 400s default max-stale).
fn seed_entry_with_catalog(
    store: &crate::discovery_cache::DiscoveryCacheStore,
    cache_key: &str,
    age_ms: u64,
    capabilities: ServerCapabilitiesDto,
    tools: Vec<McpToolDto>,
    resources: Vec<McpResourceDto>,
    prompts: Vec<McpPromptDto>,
) {
    let saved_at_ms = crate::discovery_cache::now_ms().saturating_sub(age_ms);
    store_test_entry(
        store,
        &crate::discovery_cache::DiscoveryCacheEntry::new(
            cache_key.to_string(),
            saved_at_ms,
            capabilities,
            tools,
            resources,
            vec![],
            prompts,
        ),
    );
}

fn default_test_partition_key(cache_key: &str) -> String {
    let fingerprint = crate::discovery_cache::fingerprint("grant:none");
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
        &cfg.spec,
        None,
        mcp_connection_timeout().as_millis() as u64,
    );
    crate::discovery_cache::partition_key_for_era(
        cache_key,
        &fingerprint,
        match mode {
            crate::protocol_negotiation::NegotiationMode::Auto { .. } => "modern",
            crate::protocol_negotiation::NegotiationMode::Legacy => "legacy",
        },
    )
}

fn store_test_entry(
    store: &crate::discovery_cache::DiscoveryCacheStore,
    entry: &crate::discovery_cache::DiscoveryCacheEntry,
) {
    store
        .store_partitioned(entry, &default_test_partition_key(&entry.cache_key))
        .expect("seed partitioned store");
}

fn load_test_entry(
    store: &crate::discovery_cache::DiscoveryCacheStore,
    cache_key: &str,
) -> crate::discovery_cache::EntryLookup {
    store.load_partitioned(cache_key, &default_test_partition_key(cache_key))
}

fn seed_entry(store: &crate::discovery_cache::DiscoveryCacheStore, cache_key: &str, age_ms: u64) {
    seed_entry_with_catalog(
        store,
        cache_key,
        age_ms,
        ServerCapabilitiesDto {
            tools: true,
            resources: false,
            prompts: false,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: "srv".into(),
            tool_name: "alpha".into(),
            description: "alpha tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: "mcp__srv__alpha".into(),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        }],
        vec![],
        vec![],
    );
}

/// The headline Stage 2 claim: a `Fresh` cache hit serves the cached
/// catalog and dials the transport ZERO times. Proven with the
/// TRANSPORT CALL COUNT (`mock.connect_calls`) — not merely "tools are
/// present", which a "dial-then-discard" bug would also satisfy.
#[tokio::test]
async fn a_fresh_cache_hit_serves_without_dialing() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0); // saved "now" — well inside the TTL.

    // The mock's OWN tools deliberately differ from the cached ones, so a
    // test that accidentally dialed live would be caught by tool identity
    // too, not just the call count.
    let mock = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let result = registry.connect(cfg).await;
    drop(env);

    assert!(result.is_ok(), "a cache hit must succeed, got {result:?}");
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        0,
        "a Fresh cache hit must dial the transport ZERO times"
    );
    {
        let conns = registry.connections.read().await;
        let McpConnectionState::Cached {
            tools,
            cache_saved_at_ms,
            ..
        } = conns.get("srv").unwrap()
        else {
            panic!("expected Cached state, got {:?}", conns.get("srv"));
        };
        assert_eq!(
            tools
                .iter()
                .map(|t| t.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha"],
            "the served catalog must be the CACHED one, not the mock's live tools"
        );
        assert!(*cache_saved_at_ms > 0);
    }
    assert!(
        registry.has_callable_server("srv").await,
        "a Cached server must report callable"
    );
    assert!(
        registry.server_metadata("srv").await.is_none(),
        "old seeded cache entries must not fabricate server identity"
    );
}

/// A `Stale` hit (past the 900s TTL but inside the 14 400s max-stale
/// window) must return the cached generation immediately, without waiting
/// for the Stage 3 background revalidation dial to finish.
#[tokio::test]
async fn a_stale_cache_hit_also_serves_without_dialing() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    // 1_000_000ms (~16.7min) > the 900_000ms default TTL, but well under
    // the 14_400_000ms default max-stale.
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let connect = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.connect(cfg).await })
    };
    let cached_id = tokio::time::timeout(Duration::from_millis(200), connect)
        .await
        .expect("stale hit must return before background dial finishes")
        .expect("join")
        .expect("stale hit succeeds");
    mock.connect_started.notified().await;

    let conns = registry.connections.read().await;
    assert!(
        matches!(
            conns.get("srv"),
            Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
        ),
        "expected Cached state, got {:?}",
        conns.get("srv")
    );
    drop(conns);

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation completes");
    drop(env);
}

#[tokio::test]
async fn a_stale_cache_hit_returns_immediately_and_revalidates_in_background() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation must start");
    {
        let conns = registry.connections.read().await;
        assert!(
            matches!(
                conns.get("srv"),
                Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
            ),
            "the stale-hit connect must return the cached generation without waiting"
        );
    }

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation completes");
    drop(env);

    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "the stale hit must trigger exactly one live revalidation dial"
    );
    let entry = match load_test_entry(&store, &cache_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("expected refreshed entry, got {other:?}"),
    };
    assert_eq!(
        entry.consecutive_refresh_failures, 0,
        "a successful background refresh must clear strikes"
    );
    assert_eq!(
        entry
            .tools
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect::<Vec<_>>(),
        vec!["fresh_live"],
        "the background refresh must atomically replace the cached catalog"
    );
}

#[tokio::test]
async fn a_stale_cache_hit_background_tools_failure_preserves_cached_tools_without_a_strike() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = match load_test_entry(&store, &cache_key) {
                crate::discovery_cache::EntryLookup::Found(entry) => entry,
                other => panic!("expected seeded entry, got {other:?}"),
            };
            let upgraded = matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { connection_id, tools, .. })
                    if *connection_id != cached_id
                        && tools.iter().any(|tool| tool.tool_name == "alpha")
            );
            if upgraded && entry.consecutive_refresh_failures == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background partial refresh installs a live connection");
    drop(env);

    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "the background partial refresh must still attempt one dial"
    );
    let entry = match load_test_entry(&store, &cache_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("expected refreshed cache entry, got {other:?}"),
    };
    assert_eq!(entry.consecutive_refresh_failures, 0);
    assert_eq!(
        entry
            .tools
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha"],
        "previous-safe tools must be preserved in the refreshed cache entry"
    );
}

#[tokio::test]
async fn background_revalidation_owner_is_shared_with_a_foreground_waiter() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry.connect(cfg).await.expect("stale hit connect");
    mock.connect_started.notified().await;
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
        ),
        "background-first owner must keep the cached generation visible while foreground joins"
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "foreground join must not open a second transport when background already owns the slot"
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let live_id = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("foreground waiter finishes")
        .expect("waiter join succeeds")
        .expect("foreground waiter receives L1");
    drop(env);

    assert_ne!(live_id, cached_id);
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "background and foreground must share one exact-key owner"
    );
}

#[tokio::test]
async fn background_start_detects_an_existing_foreground_owner_without_redialing() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("fresh cache hit");
    let waiter = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    mock.connect_started.notified().await;
    let lifecycle = registry.lifecycle_lock("srv");
    let _guard = lifecycle.lock().await;
    let prepare = registry
        .prepare_lazy_upgrade_slot_locked(
            "srv",
            LazyUpgradeMode::Background,
            None,
            None,
            crate::protocol_negotiation::NegotiationMode::Legacy,
        )
        .await
        .expect("background probe");
    drop(_guard);
    assert!(
        matches!(prepare, LazyUpgradePreparation::Wait(_, false)),
        "background startup must detect the existing foreground owner and refrain from spawning another dial"
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "existing foreground owner already holds the only live dial"
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    waiter.await.unwrap().expect("foreground owner completes");
    drop(env);
}

// Claude Code 2.1.286: Ma/wo (src_210493918.js @102151/@97307),
// cached-row failure adoption (src_202066919.js @291749), and WRt
// (src_193120212.js @24956). These exercise real cache -> dial -> settlement
// paths, including disk reuse by a new registry, rather than only the writer.
#[tokio::test]
async fn fresh_cached_dial_failure_purges_entry_and_rebuilt_registry_dials_live() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let config = http_cfg("srv", "https://mcp.example.com/v1");
    let key = crate::discovery_cache::logical_cache_key(&config);
    seed_entry(&store, &key, 0);
    let mock = Arc::new(BridgeMock::new(&["rebuilt"]));
    mock.connect_failures_remaining.store(1, Ordering::SeqCst);
    let make_registry = || {
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()))
    };
    let registry = make_registry();
    let cached_id = registry
        .connect(config.clone())
        .await
        .expect("fresh cache hit");
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
    let mut events = registry.subscribe_catalog_changes();
    assert!(
        registry.ensure_connected_client("srv").await.is_err(),
        "cached dial fails"
    );
    assert_eq!(
        load_test_entry(&store, &key),
        crate::discovery_cache::EntryLookup::Absent
    );
    assert!(registry.servers_with_tools().await.is_empty());
    assert!(registry.connected_prompts().await.is_empty());
    assert!(registry.cached_discovery_contexts.read().await.is_empty());
    assert!(matches!(registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Disconnected { last_error: Some(error), .. })
            if error.contains("forced connect failure")));
    let event = events.try_recv().expect("catalog retirement");
    assert_eq!(event.retired_connection_id, Some(cached_id));
    assert!(
        events.try_recv().is_err(),
        "one owner publishes one retirement"
    );

    let rebuilt = make_registry();
    let live_id = rebuilt
        .connect(config)
        .await
        .expect("new registry dials live");
    assert_ne!(live_id, cached_id);
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);
    assert!(matches!(rebuilt.connections.read().await.get("srv"),
        Some(McpConnectionState::Connected { tools, .. }) if tools[0].tool_name == "rebuilt"));
    assert!(
        matches!(load_test_entry(&store, &key), crate::discovery_cache::EntryLookup::Found(entry)
        if entry.consecutive_refresh_failures == 0 && entry.tools[0].tool_name == "rebuilt")
    );
}

#[tokio::test]
async fn stale_cached_failure_with_foreground_waiter_retires_catalog_and_strikes_once() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");
    env.set(crate::discovery_cache::ENV_STRIKES, "3");
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let config = http_cfg("srv", "https://mcp.example.com/v1");
    let key = crate::discovery_cache::logical_cache_key(&config);
    seed_entry(&store, &key, 1_000_000);
    let crate::discovery_cache::EntryLookup::Found(mut entry) = load_test_entry(&store, &key)
    else {
        panic!("seeded catalog");
    };
    entry.prompts.push(prompt("cached-command"));
    entry
        .resources
        .push(resource("cached-resource", "test://cached"));
    store_test_entry(&store, &entry);
    let mock = Arc::new(BridgeMock::new(&[]));
    mock.block_connect.store(true, Ordering::SeqCst);
    mock.connect_failures_remaining.store(1, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
    let cached_id = registry.connect(config).await.expect("stale cache hit");
    tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
        .await
        .expect("cached dial reaches transport");
    assert_eq!(registry.servers_with_tools().await, ["srv"]);
    assert_eq!(registry.connected_prompts().await.len(), 1);
    let mut events = registry.subscribe_catalog_changes();
    let mut foreground = Box::pin(registry.ensure_dialed_from_cache("srv"));
    assert!(futures_util::poll!(foreground.as_mut()).is_pending());
    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let error = foreground.await.expect_err("shared cached dial fails");
    assert!(error.to_string().contains("forced connect failure"));
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
    assert!(registry.servers_with_tools().await.is_empty());
    assert!(registry.connected_prompts().await.is_empty());
    assert!(matches!(
        registry.connections.read().await.get("srv"),
        Some(McpConnectionState::Disconnected { .. })
    ));
    assert!(
        matches!(load_test_entry(&store, &key), crate::discovery_cache::EntryLookup::Found(entry)
        if entry.consecutive_refresh_failures == 1)
    );
    assert_eq!(
        events.try_recv().expect("retirement").retired_connection_id,
        Some(cached_id)
    );
    assert!(events.try_recv().is_err());
    assert!(registry.lazy_upgrade_slots.read().await.is_empty());
}

#[tokio::test]
async fn repeated_fresh_cached_failures_delete_at_the_configured_strike_threshold() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");
    env.set(crate::discovery_cache::ENV_STRIKES, "2");
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let config = http_cfg("srv", "https://mcp.example.com/v1");
    let key = crate::discovery_cache::logical_cache_key(&config);
    seed_entry(&store, &key, 0);
    let mock = Arc::new(BridgeMock::new(&[]));
    mock.connect_failures_remaining.store(2, Ordering::SeqCst);
    for attempt in 1..=2 {
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        registry
            .connect(config.clone())
            .await
            .expect("fresh cache hit below threshold");
        registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect_err("cached dial failure");
        let persisted = load_test_entry(&store, &key);
        if attempt == 1 {
            assert!(
                matches!(persisted, crate::discovery_cache::EntryLookup::Found(entry)
                if entry.consecutive_refresh_failures == 1)
            );
        } else {
            assert_eq!(persisted, crate::discovery_cache::EntryLookup::Absent);
        }
    }
    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cached_failure_after_grant_rotation_preserves_both_identity_partitions() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");
    for age_ms in [0, 1_000_000] {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let mut config = http_cfg("srv", "https://mcp.example.com/v1");
        let McpTransportSpec::Http { oauth, .. } = &mut config.spec else {
            unreachable!()
        };
        *oauth = Some(lingxi_core::host::McpOAuthConfigDto {
            client_id: None,
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });
        let storage =
            Arc::new(XaaMemStorage::default()) as Arc<dyn lingxi_core::host::SecureStorage>;
        let clock =
            Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn lingxi_core::host::Clock>;
        let oauth_key = oauth::server_key(&config.name, &config.spec);
        let save_grant = |refresh: &str| {
            let storage = storage.clone();
            let clock = clock.clone();
            let oauth_key = oauth_key.clone();
            let refresh = refresh.to_string();
            async move {
                oauth::store_tokens(
                    &storage,
                    &clock,
                    &oauth_key,
                    &oauth::StoredTokens {
                        access_token: "test-access".into(),
                        refresh_token: Some(refresh),
                        expires_at_unix: 3600,
                        client_id: None,
                        client_secret: None,
                        step_up_scope: None,
                    },
                )
                .await
                .expect("save grant");
            }
        };
        save_grant("old-grant").await;
        let mock = Arc::new(BridgeMock::new(&[]));
        mock.block_connect.store(true, Ordering::SeqCst);
        mock.connect_failures_remaining.store(1, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_oauth(OAuthDeps {
            http: GatedXaaHttp::new() as Arc<dyn lingxi_core::host::HttpTransport>,
            clock: clock.clone(),
            storage: storage.clone(),
            on_authorization_url: Arc::new(|_| {}),
            xaa_config: None,
        })
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            None,
            mcp_connection_timeout().as_millis() as u64,
        );
        let old_partition = registry
            .discovery_cache_partition_for(&config, mode)
            .await
            .expect("old grant partition");
        seed_entry(&store, &old_partition.logical_key, age_ms);
        let crate::discovery_cache::EntryLookup::Found(entry) =
            load_test_entry(&store, &old_partition.logical_key)
        else {
            panic!("seed entry");
        };
        store
            .store_partitioned(&entry, &old_partition.partition_key)
            .expect("seed old grant");
        registry
            .connect(config.clone())
            .await
            .expect("old grant cache hit");
        // A fresh cache may sit idle across a grant rotation; a stale refresh
        // instead captures its grant before the in-flight dial is released.
        if age_ms == 0 {
            save_grant("new-grant").await;
        }
        let mut foreground = Box::pin(registry.ensure_dialed_from_cache("srv"));
        assert!(futures_util::poll!(foreground.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
            .await
            .expect("cached dial reaches transport");
        if age_ms != 0 {
            save_grant("new-grant").await;
        }
        let new_partition = registry
            .discovery_cache_partition_for(&config, mode)
            .await
            .expect("new grant partition");
        assert_ne!(old_partition.partition_key, new_partition.partition_key);
        store
            .store_partitioned(&entry, &new_partition.partition_key)
            .expect("seed new grant");
        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        foreground.await.expect_err("old cached dial failed");
        for partition in [&old_partition, &new_partition] {
            assert!(
                matches!(store.load_partitioned(&partition.logical_key, &partition.partition_key),
                crate::discovery_cache::EntryLookup::Found(entry) if entry.consecutive_refresh_failures == 0),
                "a rotated grant refuses the old failure strike altogether"
            );
        }
        assert!(registry.servers_with_tools().await.is_empty());
    }
}

#[tokio::test]
async fn cached_auth_failure_adopts_needs_auth_and_purges_rejected_catalog() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");
    // Auth purge precedes and is independent of WRt's ordinary strike limit.
    env.set(crate::discovery_cache::ENV_STRIKES, "3");
    for (age_ms, status) in [(0, 401), (1_000_000, 403)] {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let config = http_cfg("srv", "https://mcp.example.com/v1");
        let key = crate::discovery_cache::logical_cache_key(&config);
        seed_entry(&store, &key, age_ms);
        let mock = Arc::new(BridgeMock::new(&[]));
        mock.block_connect.store(true, Ordering::SeqCst);
        mock.connect_auth_status.store(status, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        registry.connect(config).await.expect("cache hit");
        let mut foreground = Box::pin(registry.ensure_dialed_from_cache("srv"));
        assert!(futures_util::poll!(foreground.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
            .await
            .expect("cached dial reaches transport");
        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        foreground.await.expect_err("authentication required");
        let state = registry
            .connections
            .read()
            .await
            .get("srv")
            .cloned()
            .expect("settled state");
        assert!(matches!(state, McpConnectionState::NeedsAuth { .. }));
        assert_eq!(
            project_action_state(&state),
            lingxi_core::host::McpActionState::NeedsAuth
        );
        assert!(registry.servers_with_tools().await.is_empty());
        assert!(registry.servers_pending().await.is_empty());
        assert_eq!(
            load_test_entry(&store, &key),
            crate::discovery_cache::EntryLookup::Absent
        );
    }
}

#[tokio::test]
async fn foreground_lazy_upgrade_panic_recovers_to_disconnected_and_unblocks_waiters() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.panic_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        registry.ensure_dialed_from_cache("srv"),
    )
    .await
    .expect("waiter completes after panic")
    .unwrap_err();
    drop(env);

    assert!(
        error
            .to_string()
            .contains("cached lazy-upgrade task panicked"),
        "panic recovery must surface a terminal waiter error"
    );
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { .. })
        ),
        "foreground panic must recover to Disconnected instead of stranding Connecting"
    );
    assert!(
        registry.lazy_upgrade_slots.read().await.is_empty(),
        "panic recovery must clear the detached owner slot"
    );
}

#[tokio::test]
async fn foreground_initialize_panic_disconnects_the_known_transport() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.panic_initialize.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        registry.ensure_dialed_from_cache("srv"),
    )
    .await
    .expect("waiter completes after initialize panic")
    .unwrap_err();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.disconnect_calls.load(Ordering::SeqCst) == 1
                && mock.conns.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("known transport cleaned up after initialize panic");
    drop(env);

    assert!(
        error.to_string().contains("initialize"),
        "panic after connect must identify the initialize phase"
    );
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { .. })
        ),
        "foreground initialize panic must recover to Disconnected"
    );
    assert!(registry.lazy_upgrade_slots.read().await.is_empty());
}

#[tokio::test]
async fn background_lazy_upgrade_panic_retires_cached_state_and_purges_the_entry() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.panic_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("stale hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if load_test_entry(&store, &cache_key) == crate::discovery_cache::EntryLookup::Absent
                && registry.lazy_upgrade_slots.read().await.is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background panic settles");
    drop(env);

    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { .. })
        ),
        "background panic must retire the unusable cached state"
    );
}

#[tokio::test]
async fn background_post_connect_panic_disconnects_the_known_transport() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.panic_list_tools.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("stale cache hit");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let struck =
                load_test_entry(&store, &cache_key) == crate::discovery_cache::EntryLookup::Absent;
            if struck
                && registry.lazy_upgrade_slots.read().await.is_empty()
                && mock.disconnect_calls.load(Ordering::SeqCst) == 1
                && mock.conns.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("known transport cleaned up after post-connect panic");
    drop(env);

    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { .. })
        ),
        "background post-connect panic must retire the cached generation"
    );
}

#[tokio::test]
async fn stale_background_failure_does_not_strike_a_replaced_cached_generation() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    mock.connect_failures_remaining.store(1, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("stale hit connect");
    mock.connect_started.notified().await;
    let replacement_id = McpConnectionId::new();
    let lifecycle = registry.lifecycle_lock("srv");
    let _guard = lifecycle.lock().await;
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Cached {
            config: http_cfg("srv", "https://mcp.example.com/v1"),
            connection_id: replacement_id,
            server_info: None,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![McpToolDto {
                input_schema_projection: None,
                definition_projection: None,

                server_name: "srv".into(),
                tool_name: "replacement".into(),
                description: "replacement".into(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__replacement".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            cache_saved_at_ms: 1,
            age_ms: 1,
        },
    );
    seed_entry_with_catalog(
        &store,
        &cache_key,
        1,
        ServerCapabilitiesDto {
            tools: true,
            resources: false,
            prompts: false,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        },
        vec![McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            server_name: "srv".into(),
            tool_name: "replacement".into(),
            description: "replacement".into(),
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: "mcp__srv__replacement".into(),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        }],
        vec![],
        vec![],
    );
    drop(_guard);

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = match load_test_entry(&store, &cache_key) {
                crate::discovery_cache::EntryLookup::Found(entry) => entry,
                other => panic!("expected entry, got {other:?}"),
            };
            if entry
                .tools
                .iter()
                .any(|tool| tool.tool_name == "replacement")
                && entry.consecutive_refresh_failures == 0
                && registry.lazy_upgrade_slots.read().await.is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement entry survives old background failure");
    drop(env);

    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == replacement_id
        ),
        "the replacement cached generation must remain current"
    );
}

#[tokio::test]
async fn rejected_background_cleanup_does_not_hold_the_lifecycle_lock() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    mock.block_disconnect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry.connect(cfg).await.expect("stale hit connect");
    mock.connect_started.notified().await;
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Cached {
            config: http_cfg("srv", "https://mcp.example.com/v2"),
            connection_id: cached_id,
            server_info: None,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            cache_saved_at_ms: 1,
            age_ms: 1_000_000,
        },
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    mock.disconnect_started.notified().await;

    tokio::time::timeout(Duration::from_millis(200), registry.remove("srv"))
        .await
        .expect("remove must not wait for rejected cleanup disconnect")
        .expect("remove succeeds while cleanup is blocked");
    assert!(
        registry.connections.read().await.get("srv").is_none(),
        "remove should acquire the lifecycle lock even while cleanup waits on transport disconnect"
    );

    mock.block_disconnect.store(false, Ordering::SeqCst);
    mock.disconnect_release.notify_one();
    drop(env);
}

#[tokio::test]
async fn concurrent_stale_cache_hits_share_one_background_revalidation() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let a = {
        let registry = registry.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { registry.connect(cfg).await })
    };
    let b = {
        let registry = registry.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { registry.connect(cfg).await })
    };
    let (a, b) = tokio::join!(a, b);
    let id_a = a.unwrap().expect("first stale hit");
    let id_b = b.unwrap().expect("second stale hit");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shared background revalidation must start");
    assert_eq!(
        id_a, id_b,
        "both callers must receive the same cached generation"
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "concurrent stale-hit connects must single-flight the background dial"
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation completes");
    drop(env);
}

#[tokio::test]
async fn catalog_refresh_snapshot_recovers_missed_cached_registration() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["live"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
    let mut active = std::collections::HashSet::new();
    apply_lagged_tool_recovery(&registry, &mut active)
        .await
        .expect("snapshot recovery");

    assert_eq!(active, std::collections::HashSet::from([cached_id]));
    drop(env);
}

#[tokio::test]
async fn catalog_refresh_snapshot_excludes_agent_scoped_entries() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["live"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let shared_id = registry.connect(cfg).await.expect("shared fresh cache hit");
    let scoped_id = McpConnectionId::new();
    registry.connections.write().await.insert(
        "agent:test:srv".into(),
        McpConnectionState::Cached {
            config: http_cfg("srv", "https://mcp.example.com/v1"),
            connection_id: scoped_id,
            server_info: None,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![McpToolDto {
                input_schema_projection: None,
                definition_projection: None,

                server_name: "srv".into(),
                tool_name: "scoped".into(),
                description: "scoped".into(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__scoped".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            cache_saved_at_ms: 1,
            age_ms: 0,
        },
    );

    let mut active = std::collections::HashSet::from([scoped_id]);
    apply_lagged_tool_recovery(&registry, &mut active)
        .await
        .expect("snapshot recovery");

    assert_eq!(active, std::collections::HashSet::from([shared_id]));
    drop(env);
}

#[tokio::test]
async fn catalog_refresh_snapshot_recovers_missed_cached_retirement_after_lazy_dial_success() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["live"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
    let live_id = registry
        .ensure_dialed_from_cache("srv")
        .await
        .expect("lazy dial succeeds");
    let (connection, peer_tx, peer_rx) = drivable_connection();
    let live_client =
        Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), connection).await);
    registry.clients.write().await.insert(
        "srv".into(),
        RegisteredClient {
            connection_id: Some(live_id),
            client: live_client,
        },
    );
    let responder = spawn_tools_list_response(peer_tx, peer_rx, "live");
    let mut active = std::collections::HashSet::from([cached_id]);
    apply_lagged_tool_recovery(&registry, &mut active)
        .await
        .expect("snapshot recovery");
    responder.await.expect("tools/list responder");

    assert_eq!(active, std::collections::HashSet::from([live_id]));
    assert!(!active.contains(&cached_id));
    drop(env);
}

#[tokio::test]
async fn catalog_refresh_snapshot_recovers_missed_cached_retirement_after_lazy_dial_partial_failure(
) {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::with_drivable_calls(&["live"]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
    let live_id = registry
        .ensure_dialed_from_cache("srv")
        .await
        .expect("lazy dial keeps the transport live on tools/list failure");
    let (peer_tx, peer_rx) = mock
        .take_tool_call_peer(live_id)
        .expect("live generation must expose a drivable client");
    let responder = spawn_tools_list_response(peer_tx, peer_rx, "live");
    let mut active = std::collections::HashSet::from([cached_id]);
    apply_lagged_tool_recovery(&registry, &mut active)
        .await
        .expect("snapshot recovery");
    responder.await.expect("tools/list responder");

    assert_eq!(
        active,
        std::collections::HashSet::from([live_id]),
        "snapshot recovery must retire the cached generation and keep the live one"
    );
    drop(env);
}

#[tokio::test]
async fn background_revalidation_does_not_revive_a_removed_server() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation started");

    let remove = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.remove("srv").await })
    };
    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    remove.await.unwrap().expect("remove after background");
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    drop(env);

    assert!(
        registry.connections.read().await.get("srv").is_none(),
        "queued remove must win over the background refresh"
    );
}

#[tokio::test]
async fn background_revalidation_cas_rejects_a_reconfigured_cached_state() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation started");

    let mut reconfigured = http_cfg("srv", "https://mcp.example.com/v1");
    reconfigured.timeout_ms = Some(1234);
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Cached {
            config: reconfigured,
            connection_id: cached_id,
            server_info: None,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![McpToolDto {
                input_schema_projection: None,
                definition_projection: None,

                server_name: "srv".into(),
                tool_name: "old".into(),
                description: "old tool".into(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__old".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            cache_saved_at_ms: 1,
            age_ms: 1_000_000,
        },
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.disconnect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("discarded background transport is torn down");
    drop(env);

    let conns = registry.connections.read().await;
    let McpConnectionState::Cached { config, tools, .. } =
        conns.get("srv").expect("cached state remains")
    else {
        panic!("reconfigured cached state must remain cached")
    };
    assert_eq!(config.timeout_ms, Some(1234));
    assert_eq!(tools[0].tool_name, "old");
    drop(conns);
    let entry = match load_test_entry(&store, &cache_key) {
        crate::discovery_cache::EntryLookup::Found(entry) => entry,
        other => panic!("seeded cache entry must survive, got {other:?}"),
    };
    assert_eq!(
        entry
            .tools
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha"],
        "CAS failure must not overwrite the persisted cache entry"
    );
}

#[tokio::test]
async fn background_cas_reject_disconnect_retries_until_success() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    mock.disconnect_failures_remaining
        .store(1, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation started");

    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Cached {
            config: http_cfg("srv", "https://mcp.example.com/v2"),
            connection_id: cached_id,
            server_info: None,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            cache_saved_at_ms: 1,
            age_ms: 1_000_000,
        },
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let live_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pending = registry.pending_transport_cleanups.read().await;
            if let Some(connection_id) = pending.keys().copied().next() {
                break connection_id;
            }
            drop(pending);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect failure queued for retry");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if registry
                .pending_transport_cleanups
                .read()
                .await
                .contains_key(&live_connection_id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect failure queued for retry");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if registry.pending_transport_cleanups.read().await.is_empty()
                && mock.conns.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry cleanup succeeds");

    assert_eq!(
        mock.disconnect_calls.load(Ordering::SeqCst),
        2,
        "cleanup must retry after the first disconnect failure"
    );
    assert!(
        registry.pending_transport_cleanups.read().await.is_empty(),
        "successful retry must clear the pending cleanup entry"
    );
    drop(env);
}

#[tokio::test]
async fn background_cleanup_retries_stop_and_can_be_kicked_again() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 1_000_000);

    let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
    mock.block_connect.store(true, Ordering::SeqCst);
    mock.disconnect_failures_remaining
        .store(usize::MAX, Ordering::SeqCst);
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    let cached_id = registry
        .connect(cfg)
        .await
        .expect("stale cache hit connect");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation started");

    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Cached {
            config: http_cfg("srv", "https://mcp.example.com/v2"),
            connection_id: cached_id,
            server_info: None,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            cache_saved_at_ms: 1,
            age_ms: 1_000_000,
        },
    );

    mock.block_connect.store(false, Ordering::SeqCst);
    mock.connect_release.notify_one();
    let live_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pending = registry.pending_transport_cleanups.read().await;
            if let Some((connection_id, entry)) = pending.iter().next() {
                if !entry.retrying {
                    break *connection_id;
                }
            }
            drop(pending);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded retries end and leave a pending entry");

    assert!(
        registry
            .pending_transport_cleanups
            .read()
            .await
            .contains_key(&live_connection_id),
        "permanent disconnect failure must leave an observable pending cleanup"
    );
    assert_eq!(
        mock.disconnect_calls.load(Ordering::SeqCst),
        6,
        "cleanup should stop after one immediate disconnect and five retries"
    );

    mock.disconnect_failures_remaining
        .store(0, Ordering::SeqCst);
    registry.kick_pending_transport_cleanups().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if registry.pending_transport_cleanups.read().await.is_empty()
                && mock.conns.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("later kick retries and clears the pending cleanup");
    drop(env);
}

#[tokio::test(start_paused = true)]
async fn hanging_cleanup_disconnect_times_out_and_can_be_kicked_again() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    let connection_id = registry.connect(cfg("mock")).await.unwrap();
    mock.hang_disconnect.store(true, Ordering::SeqCst);

    let cleanup = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry.disconnect_or_schedule_cleanup(connection_id).await;
        })
    };
    mock.disconnect_started.notified().await;
    tokio::time::advance(cleanup_disconnect_timeout()).await;
    cleanup.await.expect("cleanup task joins");
    tokio::task::yield_now().await;

    drive_cleanup_retry_attempts_for_test().await;
    let mut settled = false;
    for _ in 0..64 {
        let pending = registry.pending_transport_cleanups.read().await;
        if matches!(pending.get(&connection_id), Some(entry) if !entry.retrying) {
            settled = true;
            break;
        }
        drop(pending);
        tokio::task::yield_now().await;
    }
    assert!(
        settled,
        "timed-out cleanup retries must stop and leave a retryable pending entry"
    );

    mock.hang_disconnect.store(false, Ordering::SeqCst);
    registry.kick_pending_transport_cleanups().await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(10)).await;
    tokio::task::yield_now().await;
    assert!(
        registry.pending_transport_cleanups.read().await.is_empty(),
        "a later kick must retry and clear the timed-out cleanup"
    );
}

#[tokio::test(start_paused = true)]
async fn bounded_cleanup_retry_task_does_not_hold_registry_forever() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    let weak = Arc::downgrade(&registry);
    let connection_id = registry.connect(cfg("mock")).await.unwrap();
    mock.hang_disconnect.store(true, Ordering::SeqCst);

    let cleanup = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry.disconnect_or_schedule_cleanup(connection_id).await;
        })
    };
    mock.disconnect_started.notified().await;
    tokio::time::advance(cleanup_disconnect_timeout()).await;
    cleanup.await.expect("cleanup task joins");
    tokio::task::yield_now().await;
    drop(registry);

    drive_cleanup_retry_attempts_for_test().await;
    let mut released = false;
    for _ in 0..64 {
        if weak.upgrade().is_none() {
            released = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        released,
        "bounded cleanup retries must release the registry once they stop retrying"
    );
}

#[tokio::test]
async fn connect_all_disabled_seed_does_not_override_live_state() {
    let mock = Arc::new(BridgeMock::new(&["read"]));
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    ));
    let lifecycle = registry.lifecycle_lock("srv");
    let guard = lifecycle.lock().await;

    let connect_all = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .connect_all(vec![McpServerConfig {
                    disabled: true,
                    ..cfg("srv")
                }])
                .await
        })
    };
    tokio::task::yield_now().await;

    let live_id = McpConnectionId::new();
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: cfg("srv"),
            connection_id: live_id,
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            connected_at: SystemTime::now(),
        },
    );
    drop(guard);

    assert!(
        connect_all.await.expect("join").is_empty(),
        "disabled connect_all entries remain skipped"
    );
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { connection_id, .. }) if *connection_id == live_id
        ),
        "disabled seeding must not overwrite a concurrently-live generation"
    );
}

#[tokio::test]
async fn a_fresh_cache_hit_does_not_start_background_revalidation() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry
        .connect(cfg)
        .await
        .expect("fresh cache hit connect");
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    drop(env);

    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        0,
        "a Fresh cache hit must not schedule a background revalidation dial"
    );
}

#[tokio::test]
async fn refresh_catalog_treats_resource_only_tools_change_as_rebuild_signal() {
    let mock = Arc::new(BridgeMock::new(&[]));
    mock.list_tools_fails.store(true, Ordering::SeqCst);
    let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
    let connection_id = McpConnectionId::new();
    registry.connections.write().await.insert(
        "srv".into(),
        McpConnectionState::Connected {
            config: http_cfg("srv", "https://mcp.example.com/v1"),
            connection_id,
            capabilities: ServerCapabilitiesDto {
                tools: false,
                resources: true,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            connected_at: SystemTime::now(),
        },
    );

    assert_eq!(
        registry
            .refresh_catalog(&McpCatalogChanged {
                server_name: "srv".into(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            })
            .await
            .expect("resource-only tools refresh should short-circuit"),
        Some(connection_id),
        "lag recovery must still rebuild the resource-tool partition for a resource-only server"
    );
}

#[tokio::test]
async fn connected_generation_is_not_visible_without_matching_client() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let mock = Arc::new(BridgeMock::with_resource_templates(vec![
        lingxi_core::host::McpResourceTemplateDto {
            uri_template: "file:///{path}".into(),
            name: "tmpl".into(),
            description: None,
            mime_type: None,
            annotations: None,
            meta: None,
        },
    ]));
    mock.block_resource_templates.store(true, Ordering::SeqCst);
    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    let clients_guard = registry.clients.read().await;
    let connect = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .connect(http_cfg("srv", "https://mcp.example.com/v1"))
                .await
        })
    };

    mock.resource_templates_started.notified().await;
    mock.block_resource_templates.store(false, Ordering::SeqCst);
    mock.resource_templates_release.notify_one();
    let visible_while_client_locked = tokio::time::timeout(Duration::from_millis(50), async {
        loop {
            match registry.connections.read().await.get("srv") {
                Some(McpConnectionState::Connected { .. }) => break,
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await;
    assert!(
        visible_while_client_locked.is_err(),
        "a Connected generation must not publish before its client can publish"
    );
    drop(clients_guard);

    let connection_id = connect.await.unwrap().expect("connect succeeds");
    assert!(
        matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { connection_id: current, .. }) if *current == connection_id
        ),
        "state publishes once the client lock is released"
    );
    assert!(
        registry.get_client("srv").await.is_some(),
        "a visible Connected generation must have a matching client"
    );
    drop(env);
}

#[tokio::test]
async fn builders_reject_mutation_after_first_connect_attempt() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["live"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry
        .connect_all(vec![McpServerConfig {
            disabled: true,
            ..cfg.clone()
        }])
        .await;
    assert_eq!(
        registry.set_disabled("srv", false).await.unwrap(),
        Some(lingxi_core::host::McpActionState::Connected)
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        registry.with_headers_helper_cwd(std::path::PathBuf::from("/tmp/other"))
    }));
    assert!(
        result.is_err(),
        "builders must reject post-connect mutation"
    );
    drop(env);
}

/// The other half of the Stage 2 claim: the FIRST tool call against a
/// `Cached` server dials the transport EXACTLY ONCE (the lazy dial),
/// after which the state is a real `Connected` — not still `Cached`,
/// and not re-dialed a second time by the same call.
#[tokio::test]
async fn first_tool_call_against_a_cached_server_dials_exactly_once() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry.connect(cfg).await.expect("cache hit connect");
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        0,
        "precondition: the cache hit must not have dialed yet"
    );

    // Dispatch a tool call. The mock's paired jsonrpc connection has no
    // live peer (see `paired_connection`'s doc), so the RPC itself may
    // fail — this test only asserts that the DIAL happened, not that the
    // round-trip succeeded.
    let _ = registry
        .call_tool_with_auth_retry("srv", "mcp__srv__alpha", lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!({})), None, None)
        .await;
    drop(env);

    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "the first tool call against a Cached server must dial EXACTLY ONCE"
    );
    let conns = registry.connections.read().await;
    assert!(
        matches!(conns.get("srv"), Some(McpConnectionState::Connected { .. })),
        "the lazy dial must upgrade Cached to a real Connected, got {:?}",
        conns.get("srv")
    );
}

#[tokio::test]
async fn ensure_connected_client_rereads_the_live_client_after_publish_race() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let miss_hook = Arc::new(TestPauseHook::default());
    let publish_hook = Arc::new(TestPauseHook::default());
    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()))
        .with_pause_after_initial_client_miss(miss_hook.clone())
        .with_pause_before_client_publish(publish_hook.clone()),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let clients_guard = registry.clients.read().await;
    let caller = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_connected_client("srv").await })
    };
    miss_hook.entered.notified().await;

    let owner = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    publish_hook.entered.notified().await;
    assert!(
        !caller.is_finished(),
        "caller must be paused after its initial client miss"
    );

    miss_hook.release.notify_one();
    assert!(
        !caller.is_finished(),
        "caller must still be blocked while publish holds the connections writer"
    );

    publish_hook.release.notify_one();
    drop(clients_guard);

    let live_id = tokio::time::timeout(Duration::from_secs(2), owner)
        .await
        .expect("owner finishes")
        .expect("owner join succeeds")
        .expect("owner publishes L1");
    let client = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("caller finishes")
        .expect("caller join succeeds")
        .expect("caller observes the consistency re-read");
    let published = registry
        .get_client("srv")
        .await
        .expect("published live client");
    drop(env);

    assert!(
        Arc::ptr_eq(&client, &published),
        "consistency re-read must return the L1 client that publish inserted"
    );
    assert_eq!(
        registry
            .clients
            .read()
            .await
            .get("srv")
            .and_then(|entry| entry.connection_id),
        Some(live_id)
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "the race recovery must not redial"
    );
}

#[tokio::test]
async fn call_tool_rereads_the_live_client_after_publish_race() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let miss_hook = Arc::new(TestPauseHook::default());
    let publish_hook = Arc::new(TestPauseHook::default());
    let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()))
        .with_pause_after_initial_client_miss(miss_hook.clone())
        .with_pause_before_client_publish(publish_hook.clone()),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let clients_guard = registry.clients.read().await;
    let input = serde_json::json!({"city": "sf"});
    let caller = {
        let registry = registry.clone();
        let input = input.clone();
        tokio::spawn(async move {
            registry
                .call_tool_with_auth_retry("srv", "mcp__srv__alpha", lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input), None, None)
                .await
        })
    };
    miss_hook.entered.notified().await;

    let owner = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    publish_hook.entered.notified().await;
    assert!(
        !caller.is_finished(),
        "tool caller must be paused after its initial client miss"
    );
    let live_id = *mock
        .conns
        .lock()
        .unwrap()
        .keys()
        .next()
        .expect("connected raw id before publish completes");
    let (peer_tx, peer_rx) = mock
        .take_tool_call_peer(live_id)
        .expect("tool-call peer for L1");
    let responder = spawn_tool_call_response(peer_tx, peer_rx, "alpha", input.clone());

    miss_hook.release.notify_one();
    assert!(
        !caller.is_finished(),
        "tool caller must still be blocked behind publish before the consistency re-read"
    );

    publish_hook.release.notify_one();
    drop(clients_guard);

    let owner_id = tokio::time::timeout(Duration::from_secs(2), owner)
        .await
        .expect("owner finishes")
        .expect("owner join succeeds")
        .expect("owner publishes L1");
    let result = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("tool caller finishes")
        .expect("tool caller join succeeds")
        .expect("tool call succeeds through the published client");
    responder.await.expect("tool responder");
    drop(env);

    assert_eq!(owner_id, live_id);
    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "the publish-race recovery must reuse L1 instead of redialing"
    );
}

#[tokio::test]
async fn call_tool_publish_race_client_still_retries_a_first_auth_challenge() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let miss_hook = Arc::new(TestPauseHook::default());
    let publish_hook = Arc::new(TestPauseHook::default());
    let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()))
        .with_pause_after_initial_client_miss(miss_hook.clone())
        .with_pause_before_client_publish(publish_hook.clone()),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    let clients_guard = registry.clients.read().await;
    let input = serde_json::json!({"city": "sf"});
    let caller = {
        let registry = registry.clone();
        let input = input.clone();
        tokio::spawn(async move {
            registry
                .call_tool_with_auth_retry("srv", "mcp__srv__alpha", lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input), None, None)
                .await
        })
    };
    miss_hook.entered.notified().await;

    let owner = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
    };
    publish_hook.entered.notified().await;
    let first_live_id = *mock
        .conns
        .lock()
        .unwrap()
        .keys()
        .next()
        .expect("connected raw id before publish completes");
    let responder =
        spawn_tool_call_auth_then_success(mock.clone(), Some(first_live_id), "alpha", input, 401);

    miss_hook.release.notify_one();
    publish_hook.release.notify_one();
    publish_hook.release.notify_one();
    drop(clients_guard);

    let owner_id = tokio::time::timeout(Duration::from_secs(2), owner)
        .await
        .expect("owner finishes")
        .expect("owner join succeeds")
        .expect("owner publishes L1");
    let result = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("tool caller finishes")
        .expect("tool caller join succeeds")
        .expect("tool call succeeds after reconnect retry");
    responder.await.expect("auth retry responder");
    let refreshed_id = registry
        .clients
        .read()
        .await
        .get("srv")
        .and_then(|entry| entry.connection_id)
        .expect("refreshed client id");
    drop(env);

    assert_eq!(owner_id, first_live_id);
    assert_ne!(
        refreshed_id, first_live_id,
        "a first auth challenge must drive the reconnect path to a fresh generation"
    );
    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        2,
        "publish-race client recovery must still flow into the single reconnect retry"
    );
}

#[tokio::test]
async fn cached_lazy_dial_client_still_retries_a_first_auth_challenge() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry.connect(cfg).await.expect("cache hit connect");
    let input = serde_json::json!({"city": "sf"});
    let responder =
        spawn_tool_call_auth_then_success(mock.clone(), None, "alpha", input.clone(), 401);
    let result = registry
        .call_tool_with_auth_retry("srv", "mcp__srv__alpha", lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input), None, None)
        .await
        .expect("tool call succeeds after cached lazy-dial auth retry");
    responder.await.expect("auth retry responder");
    let refreshed_id = registry
        .clients
        .read()
        .await
        .get("srv")
        .and_then(|entry| entry.connection_id)
        .expect("refreshed client id");
    drop(env);

    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        2,
        "cached lazy dial plus first auth challenge must use exactly one reconnect retry"
    );
    assert!(
        mock.conns.lock().unwrap().contains_key(&refreshed_id),
        "the post-retry client must point at the refreshed generation"
    );
}

#[test]
fn session_expired_base_url_dimension_is_normalized_and_hashed() {
    let secret = http_cfg(
        "srv",
        "https://alice:password@mcp.example.com/v1/?token=secret#fragment",
    );
    let clean = http_cfg("srv", "https://mcp.example.com/v1");
    let doubled = http_cfg("srv", "https://mcp.example.com/v1//");
    let secret_hash = telemetry_mcp_server_base_url(&secret.spec).unwrap();
    let clean_hash = telemetry_mcp_server_base_url(&clean.spec).unwrap();
    let doubled_hash = telemetry_mcp_server_base_url(&doubled.spec).unwrap();
    assert_eq!(secret_hash.as_str(), clean_hash.as_str());
    assert_ne!(
        doubled_hash.as_str(),
        clean_hash.as_str(),
        "oracle removes exactly one trailing slash"
    );
    assert_eq!(secret_hash.as_str().len(), 12);
    assert!(secret_hash
        .as_str()
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit()));
}

#[tokio::test]
async fn http_tool_call_session_expired_reconnects_once_and_retries() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg).await.expect("connect");

    let input = serde_json::json!({"city": "sf"});
    let responder =
        spawn_tool_call_session_expired_then_success(mock.clone(), None, "alpha", input.clone());
    let result = registry
        .call_tool_with_auth_retry("srv", "mcp__srv__alpha", lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input), None, None)
        .await
        .expect("tool call succeeds after session-expired reconnect");
    responder.await.expect("session expired responder");

    let refreshed_id = registry
        .clients
        .read()
        .await
        .get("srv")
        .and_then(|entry| entry.connection_id)
        .expect("refreshed client id");
    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        2,
        "a session-expired tool call must use exactly one reconnect retry"
    );
    assert!(
        mock.conns.lock().unwrap().contains_key(&refreshed_id),
        "the post-retry client must point at the refreshed generation"
    );
    let events = take_test_telemetry_events();
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::SESSION_EXPIRED)
        .expect("session expired telemetry");
    assert_eq!(event.payload["errorCode"], serde_json::json!("404"));
    assert_eq!(event.payload["transportType"], serde_json::json!("http"));
    assert!(event.payload["mcpServerKeyHash"].as_str().is_some());
    let base_url_hash = event.payload["mcpServerBaseUrl"]
        .as_str()
        .expect("base URL hash");
    assert_eq!(base_url_hash.len(), 12);
    assert!(base_url_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!base_url_hash.contains("mcp.example.com"));
    assert!(event.payload.get("mcpServerName").is_none());
    assert!(event.payload.get("mcpToolName").is_none());
}

#[tokio::test]
async fn stale_session_reconnect_preserves_oauth_grant() {
    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }

        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let McpTransportSpec::Http { oauth, .. } = &mut cfg.spec else {
        unreachable!()
    };
    *oauth = Some(lingxi_core::host::McpOAuthConfigDto {
        client_id: Some("client-id".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    });
    let clock = Arc::new(FixedClock(
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
    ));
    let storage = Arc::new(XaaMemStorage::default());
    let storage_dyn = storage.clone() as Arc<dyn lingxi_core::host::SecureStorage>;
    let key = oauth::server_key(&cfg.name, &cfg.spec);
    oauth::store_tokens(
        &storage_dyn,
        &(clock.clone() as Arc<dyn lingxi_core::host::Clock>),
        &key,
        &oauth::StoredTokens {
            access_token: "still-valid".into(),
            refresh_token: Some("refresh".into()),
            expires_at_unix: 10_000,
            client_id: Some("client-id".into()),
            client_secret: None,
            step_up_scope: None,
        },
    )
    .await
    .expect("store grant");

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_oauth(OAuthDeps {
        http: Arc::new(UnusedHttp),
        clock: clock as Arc<dyn lingxi_core::host::Clock>,
        storage: storage_dyn.clone(),
        on_authorization_url: Arc::new(|_| {}),
        xaa_config: None,
    });
    registry.connect(cfg).await.expect("connect");
    registry
        .reconnect_preserving_auth("srv")
        .await
        .expect("stale-session reconnect");
    assert!(oauth::load_tokens(&storage_dyn, &key)
        .await
        .expect("load grant")
        .is_some());
}

#[tokio::test]
async fn connect_oauth_discovery_failure_emits_server_needs_auth_with_discovery_schema_cause() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    struct DiscoveryFailHttp;

    #[async_trait]
    impl lingxi_core::host::HttpTransport for DiscoveryFailHttp {
        async fn request(
            &self,
            req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            let status = if req.url.contains("oauth-protected-resource")
                || req.url.contains("oauth-authorization-server")
            {
                404
            } else {
                500
            };
            Ok(lingxi_core::types::HttpResponse {
                status,
                headers: vec![],
                body: String::new(),
                body_bytes: Vec::new(),
            })
        }

        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    clear_test_telemetry_events();

    let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let McpTransportSpec::Http { oauth, .. } = &mut cfg.spec else {
        unreachable!()
    };
    *oauth = Some(lingxi_core::host::McpOAuthConfigDto {
        client_id: Some("client-id".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    });

    let storage = Arc::new(XaaMemStorage::default());
    let storage_dyn = storage.clone() as Arc<dyn lingxi_core::host::SecureStorage>;
    let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn lingxi_core::host::Clock>;
    let server_key = oauth::server_key(&cfg.name, &cfg.spec);
    oauth::store_tokens(
        &storage_dyn,
        &clock,
        &server_key,
        &oauth::StoredTokens {
            access_token: "expired".into(),
            refresh_token: Some("refresh".into()),
            expires_at_unix: 0,
            client_id: Some("client-id".into()),
            client_secret: None,
            step_up_scope: None,
        },
    )
    .await
    .expect("store expired tokens");

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    )
    .with_oauth(OAuthDeps {
        http: Arc::new(DiscoveryFailHttp),
        clock,
        storage: storage_dyn,
        on_authorization_url: Arc::new(|_| {}),
        xaa_config: None,
    });

    let error = registry.connect(cfg).await.expect_err("connect must fail");
    assert!(
        matches!(error, McpError::OAuth(_) | McpError::Connection(_)),
        "expected oauth/path failure, got {error:?}"
    );

    let events = take_test_telemetry_events();
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::SERVER_NEEDS_AUTH)
        .expect("server needs auth telemetry");
    assert_eq!(
        event.payload["cause"],
        serde_json::json!("discovery_schema")
    );
    assert_eq!(event.payload["transport_type"], serde_json::json!("http"));
    assert!(event.payload["mcp_server_key_hash"].as_str().is_some());
}

#[tokio::test]
async fn second_auth_failure_emits_tool_call_auth_error() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();

    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg).await.expect("connect");

    let input = serde_json::json!({"city": "sf"});
    let responder = spawn_tool_call_auth_then_auth(mock.clone(), None, "alpha", input.clone(), 403);
    let error = registry
        .call_tool_with_auth_retry("srv", "mcp__srv__alpha", lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input), None, None)
        .await
        .expect_err("second auth failure must surface");
    responder.await.expect("double-auth responder");

    assert!(
        error.is_auth_response(),
        "expected auth-shaped error, got {error:?}"
    );
    let events = take_test_telemetry_events();
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::TOOL_CALL_AUTH_ERROR)
        .expect("tool call auth error telemetry");
    assert_eq!(event.payload["error_code"], serde_json::json!("403"));
    assert_eq!(
        event.payload["auth_error_kind"],
        serde_json::json!("token_expired")
    );
    assert_eq!(event.payload["transport_type"], serde_json::json!("http"));
    assert!(event.payload["mcp_server_key_hash"].as_str().is_some());
}

/// Single-flight: two CONCURRENT tool calls against the same `Cached`
/// server must dial the transport exactly ONCE between them — the
/// second caller blocks on the same per-server lifecycle lock `connect`
/// already uses, then observes `Connected` and never dials again.
#[tokio::test]
async fn concurrent_tool_calls_against_a_cached_server_dial_only_once() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = Arc::new(
        McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path())),
    );

    registry.connect(cfg).await.expect("cache hit connect");
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        0,
        "precondition: the cache hit must not have dialed yet — otherwise \
         this test cannot distinguish single-flighting the lazy dial from \
         simply never having anything left to single-flight"
    );

    let a = {
        let registry = registry.clone();
        tokio::spawn(async move {
            let _ = registry
                .call_tool_with_auth_retry(
                    "srv",
                    "mcp__srv__alpha",
                    lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!({})),
                    None,
                    None,
                )
                .await;
        })
    };
    let b = {
        let registry = registry.clone();
        tokio::spawn(async move {
            let _ = registry
                .call_tool_with_auth_retry(
                    "srv",
                    "mcp__srv__alpha",
                    lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!({})),
                    None,
                    None,
                )
                .await;
        })
    };
    let _ = tokio::join!(a, b);
    drop(env);

    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        1,
        "two concurrent tool calls against one Cached server must dial exactly ONCE"
    );
}

/// `/mcp disconnect` on a `Cached` server must transition it to
/// `Stopped` WITHOUT calling `McpTransport::disconnect` — there is no
/// live transport connection behind a cache-served entry to tear down
/// (its `connection_id` is a synthetic one; see
/// `McpConnectionState::Cached`'s doc).
#[tokio::test]
async fn disconnecting_a_cached_server_never_touches_the_transport() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["alpha"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry.connect(cfg).await.expect("cache hit connect");
    registry.disconnect("srv").await.expect("disconnect");
    drop(env);

    assert_eq!(
        mock.disconnect_calls.load(Ordering::SeqCst),
        0,
        "a Cached server's teardown must never call transport disconnect"
    );
    let conns = registry.connections.read().await;
    assert!(
        matches!(conns.get("srv"), Some(McpConnectionState::Stopped { .. })),
        "expected Stopped state, got {:?}",
        conns.get("srv")
    );
    drop(conns);
    assert!(
        !registry.has_callable_server("srv").await,
        "a stopped server must not report callable"
    );
}

/// A `Cached` server must contribute to [`McpRegistry::servers_with_tools`]
/// exactly like a `Connected` one — `AgentTool`'s required-MCP gate must
/// not refuse a subagent spawn naming a server the model's own tool list
/// already shows as available (`build_registered_mcp_tools` gets the same
/// treatment, in `tool-mcp`).
#[tokio::test]
async fn servers_with_tools_includes_a_cached_server() {
    let _guard = crate::discovery_cache::tests_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = DiscoveryCacheEnvGuard::new();
    env.set(crate::discovery_cache::ENV_ENABLED, "true");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
    let cfg = http_cfg("srv", "https://mcp.example.com/v1");
    let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
    seed_entry(&store, &cache_key, 0);

    let mock = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

    registry.connect(cfg).await.expect("cache hit connect");
    drop(env);

    assert_eq!(
        registry.servers_with_tools().await,
        vec!["srv".to_string()],
        "a Cached server's tools must count toward servers_with_tools"
    );
    assert_eq!(
        mock.connect_calls.load(Ordering::SeqCst),
        0,
        "servers_with_tools must not have triggered a dial either"
    );
}

/// Pure-function coverage of `discovery_source_emission`, the helper
/// `connect_locked_inner`'s MISS branch consults: a `Miss` emits iff
/// `crate::discovery_cache::miss_emits_discovery_source_telemetry` says
/// so, with the exact `miss_telemetry_value` string. A `Fresh`/`Stale`
/// decision is asserted `None` here too, but that is this PURE HELPER's
/// contract, not the whole connect path any more (§11 Stage 2): a real
/// cache hit is served by `serve_discovery_cache_hit`, a SEPARATE code
/// path that emits its own `"cache_fresh"`/`"cache_stale"` telemetry with
/// the real `entryAgeMs` — see
/// `a_fresh_cache_hit_serves_without_dialing_and_emits_cache_fresh`.
#[test]
fn discovery_source_emission_matches_the_miss_gate() {
    use crate::discovery_cache::{Decision, DiscoveryCacheEntry, MissReason};

    assert_eq!(
        discovery_source_emission(&Decision::Miss {
            reason: MissReason::Absent
        }),
        Some("live")
    );
    assert_eq!(
        discovery_source_emission(&Decision::Miss {
            reason: MissReason::Expired
        }),
        Some("miss_expired")
    );
    assert_eq!(
        discovery_source_emission(&Decision::Miss {
            reason: MissReason::Disabled
        }),
        None,
        "a gate-level miss must not emit"
    );
    assert_eq!(
        discovery_source_emission(&Decision::Miss {
            reason: MissReason::Transport
        }),
        None,
        "a gate-level miss must not emit"
    );
    let entry = DiscoveryCacheEntry::new(
        "k".into(),
        1,
        ServerCapabilitiesDto::default(),
        vec![],
        vec![],
        vec![],
        vec![],
    );
    assert_eq!(
        discovery_source_emission(&Decision::Fresh {
            entry: entry.clone(),
            age_ms: 1
        }),
        None,
        "this pure helper never handles a HIT — `serve_discovery_cache_hit` does"
    );
    assert_eq!(
        discovery_source_emission(&Decision::Stale { entry, age_ms: 1 }),
        None,
        "this pure helper never handles a HIT — `serve_discovery_cache_hit` does"
    );
}

/// §20a runs on the CONNECT path, not just on `McpClient::list_tools`.
///
/// `McpRegistry::connect` fills `McpConnectionState::Connected { tools }`
/// from `self.transport.list_tools(...)` — the posix transport, which
/// never touches `tool_schema`. That is the list
/// `build_registered_mcp_tools` hands the model on every desktop session;
/// `McpClient::list_tools` (where the decision already lived) is reached
/// only by `refresh_catalog`. Without the decision here a root-`anyOf`
/// schema the oracle drops is forwarded to the model verbatim, and the
/// two `mock_mcp.rs` integration tests that cover `McpClient::list_tools`
/// stay green throughout.
// Same rationale as `connect_resolves_the_schema_gate_from_the_servers_own_hostname`
// below: the §20a flags are PROCESS-global, so the guard must span the
// `connect` await — holding it across the await IS the point of the lock.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn connect_applies_the_tool_schema_decision_to_the_model_facing_list() {
    // This test asserts the DEFAULT (both gates off) behaviour by reading
    // the PROCESS-GLOBAL §20a flags, so it must hold the same lock every
    // other §20a test in this binary holds — see
    // `tool_schema::flag_test_lock`. Without it a concurrently-running
    // `tool_schema` test that sets `tengu_mcp_normalize_root_combinators`
    // makes `combo_tool` survive here and this assertion fails at random.
    let _g = crate::tool_schema::flag_test_lock();
    let mock = Arc::new(BridgeMock::with_tool_schemas(&[
        (
            "plain_tool",
            serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        ),
        (
            "combo_tool",
            serde_json::json!({"anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "object", "properties": {"b": {"type": "string"}}}
            ]}),
        ),
    ]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("combos")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("combos").unwrap() else {
        panic!("expected Connected state");
    };
    let names: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
    assert_eq!(
        names,
        vec!["plain_tool"],
        "the root-anyOf tool must be dropped from the CONNECTED tool list \
         (the normalize gate is off by default), leaving the plain tool: {tools:?}"
    );
    assert_eq!(tools[0].full_name, "mcp__combos__plain_tool");
}

/// §20b — connecting a server with two droppable tools must still leave
/// only the healthy tool in the model-facing list (the `retain_mut`
/// aggregation change must not perturb the KEEP/DROP decision itself).
/// The aggregated `tengu_mcp_degraded` payload-building itself is unit
/// tested directly on `degraded_payloads_for_server` below — NOT via a
/// tracing capture here, deliberately: `tracing::subscriber::set_default`
/// is thread-local, but callsite `Interest` caching is process-global, so
/// a concurrently-running test's subscriber can race the cache and
/// silently starve this one's captured events under `cargo test`'s
/// default parallelism (confirmed empirically: green alone under
/// `--test-threads=1`, flaky in the full suite).
#[tokio::test]
async fn connect_still_drops_both_anyof_tools_with_aggregation_wired() {
    let mock = Arc::new(BridgeMock::with_tool_schemas(&[
        (
            "plain_tool",
            serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        ),
        (
            "combo_one",
            serde_json::json!({"anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}}
            ]}),
        ),
        (
            "combo_two",
            serde_json::json!({"anyOf": [
                {"type": "object", "properties": {"b": {"type": "string"}}}
            ]}),
        ),
    ]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("degraded_combos")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("degraded_combos").unwrap() else {
        panic!("expected Connected state");
    };
    let names: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
    assert_eq!(names, vec!["plain_tool"]);
}

/// §20b — `degraded_payloads_for_server` (the pure aggregation step) maps
/// each nonzero classification bucket to exactly one payload, with the
/// right count field populated. Reverting the match arms (e.g. routing
/// `ToolSchemaUnsupported` to `normalized_count`, or emitting one payload
/// per tool instead of per bucket) is caught here with no tracing
/// dependency at all.
#[test]
fn degraded_payloads_for_server_maps_each_bucket_to_its_own_count_field() {
    use std::collections::HashMap;
    use telemetry::tengu::mcp::DegradedReason;
    use telemetry::Verified;

    let mut counts = HashMap::new();
    counts.insert(DegradedReason::ToolSchemaNormalized, 3);
    counts.insert(DegradedReason::ToolSchemaNormalizeGated, 2);
    counts.insert(DegradedReason::ToolSchemaUnsupported, 1);
    counts.insert(DegradedReason::ToolSchemaInvalid, 4);
    counts.insert(DegradedReason::ToolPropertyKeyInvalid, 5);
    counts.insert(DegradedReason::ToolSchemaInvalidGated, 6);
    counts.insert(DegradedReason::ToolPropertyKeyInvalidGated, 7);

    let mut payloads = degraded_payloads_for_server(&counts, "http", "srv");
    payloads.sort_by_key(|p| p.reason.wire_str());

    let by_reason: std::collections::HashMap<&'static str, _> = payloads
        .iter()
        .map(|p| {
            (
                p.reason.wire_str(),
                (p.normalized_count, p.skipped_count, p.kept_count),
            )
        })
        .collect();
    assert_eq!(
        payloads.len(),
        7,
        "one payload per nonzero bucket: {payloads:?}"
    );
    assert_eq!(by_reason["tool_schema_normalized"], (Some(3), None, None));
    assert_eq!(
        by_reason["tool_schema_normalize_gated"],
        (None, Some(2), None)
    );
    assert_eq!(by_reason["tool_schema_unsupported"], (None, Some(1), None));
    assert_eq!(by_reason["tool_schema_invalid"], (None, Some(4), None));
    assert_eq!(
        by_reason["tool_property_key_invalid"],
        (None, Some(5), None)
    );
    assert_eq!(
        by_reason["tool_schema_invalid_gated"],
        (None, None, Some(6))
    );
    assert_eq!(
        by_reason["tool_property_key_invalid_gated"],
        (None, None, Some(7))
    );
    for p in &payloads {
        assert_eq!(
            p.transport_type.as_ref().map(Verified::as_str),
            Some("http")
        );
        assert!(
            p.mcp_server_name.is_none(),
            "`http` is user-configurable; the oracle's HT gate drops the name"
        );
    }
}

/// ROUND-1 REGRESSION. `connected_zero_tools` is the FIRST statement of
/// the oracle's `yn` — 20 lines above the seven tool-schema counters the
/// module doc transcribed verbatim while calling that set complete. The
/// port emitted nothing for it, so the most common silent-MCP-failure
/// signal (an OAuth-pending, resources-only, or fully-filtered server)
/// was invisible.
///
/// NOTE ON COVERAGE: this pins the predicate, not the call site. The
/// aggregated event itself cannot be asserted from a `connect` test here
/// — see `connect_still_drops_both_anyof_tools_with_aggregation_wired`
/// for why this file deliberately does no tracing capture. What the call
/// site must preserve, and what review must check, is that the argument
/// is the RAW `tools.len()` read BEFORE `retain_mut` filters the list.
#[test]
fn connected_zero_tools_fires_only_on_an_empty_raw_list_with_the_tools_capability() {
    assert!(
        connected_zero_tools_fires(true, 0),
        "server advertised tools/list and returned an empty array"
    );
    assert!(
        !connected_zero_tools_fires(true, 2),
        "a NON-empty raw list never fires it, however many tools the \u{a7}20a filter \
         later drops \u{2014} those report their own drop reason instead"
    );
    assert!(
        !connected_zero_tools_fires(false, 0),
        "with no tools capability the oracle never reaches `yn`, so an empty list is \
         not a degraded signal"
    );
    assert!(!connected_zero_tools_fires(false, 3));
}

/// The reason maps to a payload with NO count field — the oracle emits
/// `{reason,transportType,mcpServerName,..._}` for it.
#[test]
fn connected_zero_tools_bucket_becomes_a_countless_payload() {
    let counts = std::collections::HashMap::from([(
        telemetry::tengu::mcp::DegradedReason::ConnectedZeroTools,
        1,
    )]);
    let payloads = degraded_payloads_for_server(&counts, "stdio", "srv");
    assert_eq!(payloads.len(), 1, "one payload for the one nonzero bucket");
    let p = &payloads[0];
    assert_eq!(p.reason.wire_str(), "connected_zero_tools");
    assert!(p.normalized_count.is_none());
    assert!(p.skipped_count.is_none());
    assert!(p.kept_count.is_none());
    assert_eq!(
        p.transport_type
            .as_ref()
            .map(telemetry::pii::Verified::as_str),
        Some("stdio")
    );
}

#[test]
fn degraded_payloads_for_server_is_empty_when_no_bucket_is_nonzero() {
    assert!(
        degraded_payloads_for_server(&std::collections::HashMap::new(), "stdio", "srv").is_empty()
    );
}

/// §20b — `server_config_invalid_payload` carries the RAW config `type`
/// string (`McpTransportSpec::kind()`, not a `protocol_negotiation.rs`
/// `Wr`-mapped label), the fixed literal `"url"` field, and passes the
/// caller's loader-vs-connect classification straight through. Both
/// `connect()` gates (`config.config_error` / `connect_time_url_error()`)
/// funnel through this one function, so a test here covers both call
/// sites' payload shape without needing to race a tracing capture
/// against `connect()`'s own dial path.
#[test]
fn server_config_invalid_payload_carries_the_raw_transport_kind_and_fixed_field() {
    use telemetry::tengu::mcp::ConfigInvalidSource;

    let cfg = http_cfg("broken", "${MISSING:-}");

    let loader = server_config_invalid_payload(&cfg, ConfigInvalidSource::Loader);
    assert_eq!(loader.transport_type.as_str(), "http");
    assert_eq!(loader.field.as_str(), "url");
    assert_eq!(loader.source.wire_str(), "loader");

    let connect = server_config_invalid_payload(&cfg, ConfigInvalidSource::Connect);
    assert_eq!(connect.source.wire_str(), "connect");
}

/// §20b — `tools_listed_payload` counts off the FINAL (post-§20a-filter)
/// list, not a raw pre-filter count, and `always_load_count` only tallies
/// `Some(true)` (a `None`/`Some(false)` tool must NOT count). Reverting
/// either the length source or the filter predicate is caught here.
#[test]
fn tools_listed_payload_counts_off_the_final_list() {
    let tools = vec![
        McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            tool_name: "a".into(),
            full_name: "mcp__srv__a".into(),
            server_name: "srv".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            search_hint: None,
            always_load: Some(true),
            requires_user_interaction: false,
        },
        McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            tool_name: "b".into(),
            full_name: "mcp__srv__b".into(),
            server_name: "srv".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            search_hint: None,
            always_load: Some(false),
            requires_user_interaction: false,
        },
        McpToolDto {
            input_schema_projection: None,
            definition_projection: None,

            tool_name: "c".into(),
            full_name: "mcp__srv__c".into(),
            server_name: "srv".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        },
    ];
    let payload = tools_listed_payload("http", std::time::Duration::from_millis(42), &tools, "srv");
    assert_eq!(payload.transport_type.as_str(), "http");
    assert_eq!(payload.list_duration_ms, 42);
    assert_eq!(payload.tool_count, 3);
    assert_eq!(
        payload.always_load_count, 1,
        "only the Some(true) tool counts"
    );
    assert_eq!(payload.discovery_source.as_str(), "live");
    // Gated: `http` is a user-configurable transport, so the oracle's
    // `HT` gate is false and `EA` drops the key entirely.
    assert!(
        payload.mcp_server_name.is_none(),
        "a user-configured server's raw name must never reach telemetry"
    );
}

/// §20a's per-server gate resolves from the connected server's URL
/// hostname (oracle `Ot(e,t)`: `new URL(t.url).hostname`). `connect` must
/// therefore hand the gate the URL off `config.spec` — otherwise the gate
/// sees `None` for every server and only a bare `"*"` entry could ever
/// enable either transform.
///
/// Written under the ORACLE's literal flag key rather than the module's
/// private constant, so a misspelling there cannot make this green.
// The §20a flags are PROCESS-global, so the guard must span both `connect`
// calls — that is the whole point of the lock, and the `mcp` lib test
// binary runs `tool_schema`'s gate tests in the same process.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn connect_resolves_the_schema_gate_from_the_servers_own_hostname() {
    let _g = crate::tool_schema::flag_test_lock();
    let flag = crate::tool_schema::ORACLE_NORMALIZE_FLAG_FOR_TEST;
    telemetry::test_set_flag_list(flag, vec!["mcp.example.com".to_string()]);

    let listed = |cfg: McpServerConfig| async move {
        let mock = Arc::new(BridgeMock::with_tool_schemas(&[(
            "combo_tool",
            serde_json::json!({"anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}}
            ]}),
        )]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let name = cfg.name.clone();
        registry.connect(cfg).await.unwrap();
        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get(&name).unwrap() else {
            panic!("expected Connected state");
        };
        tools.clone()
    };

    // The LISTED hostname: normalization applies, the tool survives with a
    // rewritten object schema and the "Input constraint:" note.
    let listed_tools = listed(http_cfg("listed", "https://mcp.example.com/v1")).await;
    assert_eq!(
        listed_tools.len(),
        1,
        "a server whose hostname is on the flag list must have its schema NORMALIZED, not dropped: {listed_tools:?}"
    );
    assert_eq!(
        listed_tools[0].input_schema["type"],
        serde_json::json!("object")
    );
    assert!(listed_tools[0].description.starts_with("Input constraint:"));

    // An UNLISTED hostname takes the gate-off branch and is dropped.
    let other_tools = listed(http_cfg("other", "https://mcp.other.org/v1")).await;
    assert!(
        other_tools.is_empty(),
        "a server off the flag list must still be dropped: {other_tools:?}"
    );

    telemetry::test_clear_flag_list(flag);
}

#[tokio::test]
async fn connect_preserves_double_underscore_tool_name() {
    let mock = Arc::new(BridgeMock::new(&["read__file"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("fs")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("fs").unwrap() else {
        panic!("expected Connected state");
    };
    // The `__` inside the tool name survives verbatim (normalize is a no-op
    // for names already matching `[a-zA-Z0-9_-]` — `_` is a valid char).
    assert_eq!(tools[0].full_name, "mcp__fs__read__file");
}

#[tokio::test]
async fn connect_normalizes_special_char_tool_segment_and_resolves_raw_wire_name() {
    // claude-code's `buildMcpToolName` normalizes BOTH the server AND the
    // tool segment (`client.ts:1768` → `mcpStringUtils.ts:51`). A tool whose
    // wire name contains a character outside `[a-zA-Z0-9_-]` (here the `.` in
    // `weather.now`) gets a NORMALIZED model-facing FQN, while the RAW wire
    // name is kept on the dto for dispatch (claude-code's `mcpInfo.toolName`).
    let mock = Arc::new(BridgeMock::new(&["weather.now"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("forecast")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("forecast").unwrap() else {
        panic!("expected Connected state");
    };
    // Model-facing FQN: the `.` normalizes to `_`.
    assert_eq!(tools[0].full_name, "mcp__forecast__weather_now");
    // The dto keeps the RAW wire name for dispatch.
    assert_eq!(tools[0].tool_name, "weather.now");
    drop(conns);

    // resolve_wire_tool_name maps the normalized model-facing FQN back to
    // the RAW wire name the server expects on `tools/call`.
    assert_eq!(
        registry
            .resolve_wire_tool_name("forecast", "mcp__forecast__weather_now", None)
            .await
            .as_deref(),
        Some("weather.now"),
    );
    // Unknown FQN or server ⇒ None (the caller falls back to the parsed
    // segment, a no-op for valid-identifier names).
    assert_eq!(
        registry
            .resolve_wire_tool_name("forecast", "mcp__forecast__missing", None)
            .await,
        None,
    );
    assert_eq!(
        registry
            .resolve_wire_tool_name("nope", "mcp__forecast__weather_now", None)
            .await,
        None,
    );
}

#[tokio::test]
async fn agent_scoped_connect_does_not_collide_with_a_shared_connect_of_the_same_name() {
    // §24b: a subagent's inline `mcpServers: {docs: ...}` must never clobber
    // (or be clobbered by) an unrelated shared/session-level "docs" server.
    let mock = Arc::new(BridgeMock::new(&["search"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    let shared_id = registry.connect(cfg("docs")).await.unwrap();
    let (scoped_id, table_key) = registry
        .connect_agent_scoped(cfg("docs"), AgentId::new())
        .await
        .unwrap();

    assert_ne!(
        shared_id, scoped_id,
        "the scoped connect must be a SEPARATE connection, not a no-op reuse of the shared one"
    );
    assert_ne!(
        table_key, "docs",
        "the scoped table key must never equal the plain server name"
    );
    // Both entries independently readable by their OWN key; `config.name`
    // ("docs") is IDENTICAL on both — the plain display name is untouched.
    let shared_cfg = registry.get_config("docs").await.unwrap();
    assert_eq!(shared_cfg.name, "docs");
    let scoped_cfg = registry.get_config(&table_key).await.unwrap();
    assert_eq!(
        scoped_cfg.name, "docs",
        "the scoped config's plain `name` field must stay unmangled"
    );
    assert!(registry.has_callable_server("docs").await);
    assert!(registry.has_callable_server(&table_key).await);
}

#[tokio::test]
async fn agent_scoped_connect_is_unreachable_by_the_plain_server_name() {
    // §24b core invariant: dispatch by the model-facing plain name must
    // NEVER accidentally resolve a private agent-scoped connection when no
    // shared server of that name exists — that would leak a subagent's
    // private server to every other caller.
    let mock = Arc::new(BridgeMock::new(&["search"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    let (_id, table_key) = registry
        .connect_agent_scoped(cfg("docs"), AgentId::new())
        .await
        .unwrap();

    assert!(
        registry.get_config("docs").await.is_none(),
        "no SHARED \"docs\" server exists — the plain name must resolve to nothing"
    );
    assert!(
        !registry.has_callable_server("docs").await,
        "the plain name must not dispatch to the private scoped connection"
    );
    // The scoped key is the ONLY way to reach it.
    assert!(registry.get_config(&table_key).await.is_some());
    assert!(registry.has_callable_server(&table_key).await);
    assert!(registry.get_client(&table_key).await.is_some());
}

#[tokio::test]
async fn disconnect_agent_scoped_tears_down_only_its_own_connection() {
    // §24b: tearing down a subagent's OWN newly-created connection must
    // never touch an unrelated shared connection of the same plain name.
    let mock = Arc::new(BridgeMock::new(&["search"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    registry.connect(cfg("docs")).await.unwrap();
    let (_id, table_key) = registry
        .connect_agent_scoped(cfg("docs"), AgentId::new())
        .await
        .unwrap();

    registry.disconnect_agent_scoped(&table_key).await.unwrap();

    assert!(
        registry.get_config(&table_key).await.is_none(),
        "the scoped entry must be fully removed after teardown"
    );
    assert!(
        registry.has_callable_server("docs").await,
        "the UNRELATED shared \"docs\" connection must survive the scoped teardown"
    );
}

#[tokio::test]
async fn two_agent_scoped_connects_of_the_same_name_get_independent_keys() {
    // §24b: TWO concurrent subagent spawns each declaring an inline
    // `mcpServers: {docs: ...}` must not collide with EACH OTHER either
    // (not just against a shared server) — this is the exact scenario the
    // reverted `agent_scope.rs` attempt mangled the name to prevent.
    let mock = Arc::new(BridgeMock::new(&["search"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    let (id_a, key_a) = registry
        .connect_agent_scoped(cfg("docs"), AgentId::new())
        .await
        .unwrap();
    let (id_b, key_b) = registry
        .connect_agent_scoped(cfg("docs"), AgentId::new())
        .await
        .unwrap();

    assert_ne!(
        key_a, key_b,
        "distinct agent ids must get distinct table keys"
    );
    assert_ne!(id_a, id_b, "each spawn gets its own live connection");
    assert!(registry.get_config(&key_a).await.is_some());
    assert!(registry.get_config(&key_b).await.is_some());

    // Tearing down A must leave B fully intact.
    registry.disconnect_agent_scoped(&key_a).await.unwrap();
    assert!(registry.get_config(&key_a).await.is_none());
    assert!(
        registry.get_config(&key_b).await.is_some(),
        "spawn B's connection must survive spawn A's teardown"
    );
}

#[tokio::test]
async fn connect_normalizes_claudeai_prefixed_server() {
    let mock = Arc::new(BridgeMock::new(&["search"]));
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );
    // "claude.ai Linear" → normalize → "claude_ai_Linear".
    registry.connect(cfg("claude.ai Linear")).await.unwrap();

    let conns = registry.connections.read().await;
    let McpConnectionState::Connected { tools, .. } = conns.get("claude.ai Linear").unwrap() else {
        panic!("expected Connected state");
    };
    assert_eq!(tools[0].full_name, "mcp__claude_ai_Linear__search");
    drop(conns);
    assert!(registry.get_client("claude_ai_Linear").await.is_some());
}

// ---- P1-08: runtime `/add-dir` live roots + notification fan-out -------

/// Build a paired `Connection` whose PEER ends stay observable (unlike the
/// module `paired_connection`, which drops them), so a test can read the
/// frames the client emits.
fn observable_connection() -> (Arc<Connection>, mpsc::Receiver<Bytes>) {
    let (_peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
    let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
    let conn = Arc::new(Connection::new_streams(
        peer_to_us_rx,
        us_to_peer_tx,
        Mode::Lines,
    ));
    (conn, us_to_peer_rx)
}

struct PromptBridgeMock {
    conns: TestMutex<HashMap<ConnId, Arc<Connection>>>,
    inbound_txs: TestMutex<HashMap<ConnId, mpsc::Sender<Bytes>>>,
    prompt_peers: TestMutex<HashMap<ConnId, mpsc::Receiver<Bytes>>>,
    connect_calls: AtomicUsize,
    block_connect: std::sync::atomic::AtomicBool,
    connect_started: Notify,
    connect_release: Notify,
}

impl PromptBridgeMock {
    fn new() -> Self {
        Self {
            conns: TestMutex::new(HashMap::new()),
            inbound_txs: TestMutex::new(HashMap::new()),
            prompt_peers: TestMutex::new(HashMap::new()),
            connect_calls: AtomicUsize::new(0),
            block_connect: std::sync::atomic::AtomicBool::new(false),
            connect_started: Notify::new(),
            connect_release: Notify::new(),
        }
    }

    async fn wait_for_connection_id(&self) -> ConnId {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let connection_id = { self.conns.lock().unwrap().keys().next().copied() };
                if let Some(connection_id) = connection_id {
                    break connection_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("prompt connection appears")
    }

    async fn answer_next_prompt(&self, expected_connection_id: ConnId) {
        let mut peer = self
            .prompt_peers
            .lock()
            .unwrap()
            .remove(&expected_connection_id)
            .expect("peer for connected prompt server");
        let frame = tokio::time::timeout(Duration::from_secs(2), peer.recv())
            .await
            .expect("prompts/get request within timeout")
            .expect("prompts/get frame");
        let req: Value = serde_json::from_slice(&frame).expect("json request");
        assert_eq!(req["method"], "prompts/get");
        assert_eq!(req["params"]["name"], "draft");
        assert_eq!(
            req["params"]["arguments"],
            serde_json::json!({ "topic": "release" })
        );
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": req["id"].clone(),
            "result": {
                "description": "Draft prompt",
                "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
            }
        }))
        .expect("serialize prompts/get response");
        response.push(b'\n');
        let sender = self
            .inbound_txs
            .lock()
            .unwrap()
            .get(&expected_connection_id)
            .expect("live inbound sender")
            .clone();
        sender
            .send(Bytes::from(response))
            .await
            .expect("send prompts/get response");
        self.prompt_peers
            .lock()
            .unwrap()
            .insert(expected_connection_id, peer);
    }
}

#[async_trait]
impl McpTransport for PromptBridgeMock {
    async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        self.connect_started.notify_one();
        if self.block_connect.load(Ordering::SeqCst) {
            self.connect_release.notified().await;
        }
        let id = ConnId::new();
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let conn = Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ));
        self.conns.lock().unwrap().insert(id, conn);
        self.inbound_txs.lock().unwrap().insert(id, peer_to_us_tx);
        self.prompt_peers.lock().unwrap().insert(id, us_to_peer_rx);
        Ok(McpRawConnection { connection_id: id })
    }

    async fn initialize(&self, _c: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: true,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        })
    }

    async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_resources(&self, _c: &McpRawConnection) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        Ok(vec![McpPromptDto {
            name: "draft".into(),
            description: Some("draft prompt".into()),
            arguments: Vec::new(),
        }])
    }

    async fn call_tool(
        &self,
        _c: &McpRawConnection,
        _t: &str,
        _i: Value,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!("prompt mock never serves tools")
    }

    async fn read_resource(
        &self,
        _c: &McpRawConnection,
        _u: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!("prompt mock never serves resources")
    }

    async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _c: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!("prompt mock notifications unused")
    }

    async fn handle_elicitation(
        &self,
        _c: &McpRawConnection,
        _r: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        unreachable!("prompt mock elicitation unused")
    }

    async fn disconnect(&self, id: ConnId) -> Result<(), McpError> {
        self.conns.lock().unwrap().remove(&id);
        self.inbound_txs.lock().unwrap().remove(&id);
        self.prompt_peers.lock().unwrap().remove(&id);
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Http]
    }
}

impl RawConnectionProvider for PromptBridgeMock {
    fn connection_for(&self, id: ConnId) -> Option<Arc<Connection>> {
        self.conns.lock().unwrap().get(&id).cloned()
    }
}

#[test]
fn add_root_reports_change_only_on_a_real_add() {
    // jzn-style change-compare: a NEW dir returns true and lands in the
    // shared set; re-adding it returns false (a no-op, so the caller sends
    // NO roots/list_changed notification).
    let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[])));
    assert!(
        registry.add_root(std::path::PathBuf::from("/extra")),
        "first add of a dir must report a change"
    );
    assert!(
        !registry.add_root(std::path::PathBuf::from("/extra")),
        "re-adding an already-present dir must report NO change"
    );
    assert_eq!(
        registry.additional_roots_snapshot(),
        vec![std::path::PathBuf::from("/extra")],
        "the dir is stored exactly once",
    );
}

#[tokio::test]
async fn notify_roots_list_changed_all_fans_out_one_per_client() {
    // The fan-out sends exactly one `notifications/roots/list_changed` to
    // EVERY connected client (claude-code notifyMcpRootsListChanged → per-
    // client sendRootsListChanged).
    let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[])));

    let (conn_a, mut peer_a) = observable_connection();
    let (conn_b, mut peer_b) = observable_connection();
    let client_a = Arc::new(McpClient::new("a", std::path::PathBuf::from("/a"), conn_a).await);
    let client_b = Arc::new(McpClient::new("b", std::path::PathBuf::from("/b"), conn_b).await);
    registry.register_test_client("a", cfg("a"), client_a).await;
    registry.register_test_client("b", cfg("b"), client_b).await;

    let notified = registry.notify_roots_list_changed_all().await;
    assert_eq!(notified, 2, "both connected clients must be notified");

    for peer in [&mut peer_a, &mut peer_b] {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv())
            .await
            .expect("notification within timeout")
            .expect("a frame was emitted");
        let text = std::str::from_utf8(&frame).expect("utf-8 frame");
        assert!(
            text.contains(r#""method":"notifications/roots/list_changed""#),
            "each client must receive the roots/list_changed notification: {text}",
        );
        assert!(
            !text.contains(r#""id""#),
            "a notification carries no id: {text}"
        );
        // Exactly ONE frame per client — no second notification queued.
        assert!(
            peer.try_recv().is_err(),
            "a client must receive exactly one notification",
        );
    }
}

#[tokio::test]
async fn notify_roots_list_changed_all_on_empty_registry_notifies_none() {
    let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[])));
    assert_eq!(registry.notify_roots_list_changed_all().await, 0);
}

// -----------------------------------------------------------------
// §26b delta 2: XAA single-flight. `resolve_xaa_token`/
// `resolve_xaa_token_inner` are crate-private, so this lives here
// (rather than in the integration suite, `mcp/tests/oauth_flow_test.rs`)
// where it can call them directly — the public `connect()` entry point
// already serializes per server name via `lifecycle_lock`, which would
// mask whether the XAA-specific guard does anything at all.
// -----------------------------------------------------------------

struct FixedClock(std::time::SystemTime);
impl lingxi_core::host::Clock for FixedClock {
    fn now(&self) -> std::time::SystemTime {
        self.0
    }
}

#[derive(Default)]
struct XaaMemStorage {
    map: TestMutex<HashMap<(String, String), lingxi_core::types::SecureStorageData>>,
}
#[async_trait]
impl lingxi_core::host::SecureStorage for XaaMemStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: lingxi_core::types::SecureStorageData,
    ) -> Result<(), lingxi_core::host::SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .insert((service.into(), account.into()), data);
        Ok(())
    }
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<lingxi_core::types::SecureStorageData>, lingxi_core::host::SecureStorageError>
    {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&(service.into(), account.into()))
            .cloned())
    }
    async fn delete(
        &self,
        service: &str,
        account: &str,
    ) -> Result<(), lingxi_core::host::SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .remove(&(service.into(), account.into()));
        Ok(())
    }
    async fn list(
        &self,
        service: &str,
    ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, a)| a.clone())
            .collect())
    }
    fn is_encrypted(&self) -> bool {
        false
    }
    fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
        lingxi_core::host::SecureStorageBackend::PlainText
    }
}

/// XAA config provider handing back fixed IdP+AS inputs.
struct FixedXaaProvider;
#[async_trait]
impl XaaConfigProvider for FixedXaaProvider {
    async fn xaa_inputs(
        &self,
        _server_name: &str,
        _server_url: &str,
    ) -> Result<Option<XaaInputs>, McpError> {
        Ok(Some(XaaInputs {
            client_id: "as-client".into(),
            client_secret: "as-secret".into(),
            idp_client_id: "idp-client".into(),
            idp_client_secret: None,
            idp_id_token: "the-id-token".into(),
            idp_token_endpoint: "https://idp.example.com/token".into(),
        }))
    }
}

/// HTTP mock answering the XAA discovery/exchange legs; the AS
/// jwt-bearer POST (the "mint the access token" step) counts its hits
/// and sleeps briefly before completing, giving a concurrent second
/// resolve every opportunity to race ahead if the single-flight guard
/// is missing.
struct GatedXaaHttp {
    exchange_calls: std::sync::atomic::AtomicUsize,
}
impl GatedXaaHttp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            exchange_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl lingxi_core::host::HttpTransport for GatedXaaHttp {
    async fn request(
        &self,
        req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
        let url = req.url.clone();
        if url.contains("oauth-protected-resource") {
            return Ok(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com"]}"#.into(),
                body_bytes: Vec::new(),
            });
        }
        if url.contains("oauth-authorization-server") {
            return Ok(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"issuer":"https://as.example.com","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.into(),
                body_bytes: Vec::new(),
            });
        }
        if url.contains("idp.example.com/token") {
            return Ok(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"access_token":"id-jag","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag"}"#.into(),
                body_bytes: Vec::new(),
            });
        }
        if url == "https://as.example.com/token" {
            self.exchange_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Ok(lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"access_token":"xaa-access","token_type":"Bearer","expires_in":3600}"#
                    .into(),
                body_bytes: Vec::new(),
            });
        }
        Ok(lingxi_core::types::HttpResponse {
            status: 404,
            headers: vec![],
            body: String::new(),
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }
}

fn xaa_unit_test_config(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: Some(lingxi_core::host::McpOAuthConfigDto {
                client_id: Some("as-client".into()),
                callback_port: None,
                auth_server_metadata_url: None,
                scopes: None,
                xaa: Some(true),
            }),
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

/// §26b delta 2: two concurrent `resolve_xaa_token` calls for the SAME
/// server key must share one exchange. Without the `xaa_refresh_lock`
/// guard, task B (spawned once task A's exchange is confirmed in
/// flight, and given a 50ms window while A "sleeps" mid-request) would
/// independently run its own full IdP+AS chain and land on the same AS
/// jwt-bearer endpoint too, driving `exchange_calls` to 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xaa_concurrent_resolves_share_one_exchange() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    let http = GatedXaaHttp::new();
    let registry = Arc::new(McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(
        OAuthDeps {
            http: http.clone() as Arc<dyn lingxi_core::host::HttpTransport>,
            clock: Arc::new(FixedClock(
                std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            )),
            storage: Arc::new(XaaMemStorage::default())
                as Arc<dyn lingxi_core::host::SecureStorage>,
            on_authorization_url: Arc::new(|_url: &str| {}),
            xaa_config: Some(Arc::new(FixedXaaProvider)),
        },
    ));

    let config = xaa_unit_test_config("xaa-concurrent");
    let key = oauth::server_key(&config.name, &config.spec);

    let (r1, c1, k1) = (registry.clone(), config.clone(), key.clone());
    let task_a = tokio::spawn(async move {
        let deps = r1.oauth.as_ref().unwrap().clone();
        r1.resolve_xaa_token(&c1, &k1, &deps).await
    });
    let (r2, c2, k2) = (registry.clone(), config.clone(), key.clone());
    let task_b = tokio::spawn(async move {
        let deps = r2.oauth.as_ref().unwrap().clone();
        r2.resolve_xaa_token(&c2, &k2, &deps).await
    });

    let (res_a, res_b) = tokio::join!(task_a, task_b);
    std::env::remove_var("LINGXI_ENABLE_XAA");

    let tok_a = res_a.unwrap().expect("task A resolves");
    let tok_b = res_b.unwrap().expect("task B resolves");
    assert_eq!(tok_a.access_token.expose_secret(), "xaa-access");
    assert_eq!(tok_b.access_token.expose_secret(), "xaa-access");
    assert_eq!(
        http.exchange_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "single-flight: exactly one AS jwt-bearer exchange for two concurrent resolves"
    );
    let success_events: Vec<_> = take_test_telemetry_events()
        .into_iter()
        .filter(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS)
        .collect();
    assert_eq!(success_events.len(), 1);
    assert_eq!(
        success_events[0].payload,
        serde_json::json!({"authMethod":"xaa","idTokenCacheHit":false})
    );
}

#[tokio::test]
async fn xaa_issuer_mismatch_is_reported_as_discovery_failure() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    struct IssuerMismatchHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for IssuerMismatchHttp {
        async fn request(
            &self,
            req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            let (status, body) = if req.url.contains("oauth-protected-resource") {
                (
                    200,
                    r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com/root"]}"#.to_string(),
                )
            } else if req.url.contains("oauth-authorization-server") {
                (
                    200,
                    r#"{"issuer":"https://other.example.com/root","token_endpoint":"https://other.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string(),
                )
            } else {
                (404, String::new())
            };
            Ok(lingxi_core::types::HttpResponse {
                status,
                headers: vec![],
                body,
                body_bytes: Vec::new(),
            })
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(OAuthDeps {
        http: Arc::new(IssuerMismatchHttp),
        clock: Arc::new(FixedClock(
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        )),
        storage: Arc::new(XaaMemStorage::default()) as Arc<dyn lingxi_core::host::SecureStorage>,
        on_authorization_url: Arc::new(|_url: &str| {}),
        xaa_config: Some(Arc::new(FixedXaaProvider)),
    });

    let config = xaa_unit_test_config("xaa-issuer");
    let key = oauth::server_key(&config.name, &config.spec);
    let deps = registry.oauth.as_ref().unwrap().clone();
    let error = registry
        .resolve_xaa_token(&config, &key, &deps)
        .await
        .expect_err("issuer mismatch must fail");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert!(
        matches!(error, McpError::OAuth(_)),
        "unexpected error: {error:?}"
    );
    let events = take_test_telemetry_events();
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_FAILURE)
        .expect("XAA flow failure telemetry");
    assert_eq!(event.payload["authMethod"], serde_json::json!("xaa"));
    assert_eq!(
        event.payload["xaaFailureStage"],
        serde_json::json!("discovery")
    );
    assert_eq!(event.payload["idTokenCacheHit"], serde_json::json!(false));
    assert!(events
        .iter()
        .all(|event| event.name != telemetry::tengu::mcp::OAUTH_ISSUER_ECHO_MISMATCH));
}

#[test]
fn xaa_prm_failure_is_classified_as_discovery_without_error_detail() {
    let error = crate::xaa::XaaError::Prm("secret-bearing transport detail".to_string());

    assert_eq!(xaa_flow_failure_stage(&error), "discovery");
}

#[tokio::test]
async fn xaa_jwt_bearer_failure_emits_oauth_flow_failure() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    struct CacheHitXaaProvider;
    #[async_trait]
    impl XaaConfigProvider for CacheHitXaaProvider {
        async fn xaa_inputs(
            &self,
            _server_name: &str,
            _server_url: &str,
        ) -> Result<Option<XaaInputs>, McpError> {
            Ok(Some(XaaInputs {
                client_id: "as-client".into(),
                client_secret: "as-secret".into(),
                idp_client_id: "idp-client".into(),
                idp_client_secret: None,
                idp_id_token: "the-id-token".into(),
                idp_token_endpoint: "https://idp.example.com/token".into(),
            }))
        }

        async fn peek_id_token_cache_hit(
            &self,
            _server_name: &str,
            _server_url: &str,
        ) -> Result<bool, McpError> {
            Ok(true)
        }
    }

    struct JwtBearerFailureHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for JwtBearerFailureHttp {
        async fn request(
            &self,
            req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            let (status, body) = if req.url.contains("oauth-protected-resource") {
                (
                    200,
                    r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com/root"]}"#.to_string(),
                )
            } else if req.url.contains("oauth-authorization-server") {
                (
                    200,
                    r#"{"issuer":"https://as.example.com/root","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string(),
                )
            } else if req.url.contains("idp.example.com/token") {
                (
                    200,
                    "{\"access_token\":\"idp-access\",\"issued_token_type\":\"urn:ietf:params:oauth:token-type:id-jag\",\"token_type\":\"Bearer\",\"expires_in\":3600}".to_string(),
                )
            } else if req.url.contains("as.example.com/token") {
                (400, r#"{"error":"invalid_grant"}"#.to_string())
            } else {
                (404, String::new())
            };
            Ok(lingxi_core::types::HttpResponse {
                status,
                headers: vec![],
                body,
                body_bytes: Vec::new(),
            })
        }

        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(OAuthDeps {
        http: Arc::new(JwtBearerFailureHttp),
        clock: Arc::new(FixedClock(
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        )),
        storage: Arc::new(XaaMemStorage::default()) as Arc<dyn lingxi_core::host::SecureStorage>,
        on_authorization_url: Arc::new(|_url: &str| {}),
        xaa_config: Some(Arc::new(CacheHitXaaProvider)),
    });

    let config = xaa_unit_test_config("xaa-jwt-bearer");
    let key = oauth::server_key(&config.name, &config.spec);
    let deps = registry.oauth.as_ref().unwrap().clone();
    let error = registry
        .resolve_xaa_token(&config, &key, &deps)
        .await
        .expect_err("jwt-bearer failure must fail");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert!(matches!(error, McpError::OAuth(_)));
    let events = take_test_telemetry_events();
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_FAILURE)
        .expect("oauth flow failure telemetry");
    assert_eq!(event.payload["authMethod"], serde_json::json!("xaa"));
    assert_eq!(
        event.payload["xaaFailureStage"],
        serde_json::json!("jwt_bearer")
    );
    assert_eq!(event.payload["idTokenCacheHit"], serde_json::json!(true));
}

#[tokio::test]
async fn xaa_provider_discovery_failure_emits_oauth_flow_failure() {
    let _capture = test_telemetry_capture_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_test_telemetry_events();
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    struct DiscoveryFailingXaaProvider;
    #[async_trait]
    impl XaaConfigProvider for DiscoveryFailingXaaProvider {
        async fn xaa_inputs(
            &self,
            _server_name: &str,
            _server_url: &str,
        ) -> Result<Option<XaaInputs>, McpError> {
            Err(McpError::OAuth(
                "XAA IdP: OIDC discovery transport: timeout".into(),
            ))
        }

        async fn peek_id_token_cache_hit(
            &self,
            _server_name: &str,
            _server_url: &str,
        ) -> Result<bool, McpError> {
            Ok(true)
        }
    }

    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }

        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(OAuthDeps {
        http: Arc::new(UnusedHttp),
        clock: Arc::new(FixedClock(
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        )),
        storage: Arc::new(XaaMemStorage::default()) as Arc<dyn lingxi_core::host::SecureStorage>,
        on_authorization_url: Arc::new(|_url: &str| {}),
        xaa_config: Some(Arc::new(DiscoveryFailingXaaProvider)),
    });

    let config = xaa_unit_test_config("xaa-provider-discovery");
    let key = oauth::server_key(&config.name, &config.spec);
    let deps = registry.oauth.as_ref().unwrap().clone();
    let error = registry
        .resolve_xaa_token(&config, &key, &deps)
        .await
        .expect_err("provider discovery failure must fail");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert!(matches!(error, McpError::OAuth(_)));
    let events = take_test_telemetry_events();
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_FAILURE)
        .expect("oauth flow failure telemetry");
    assert_eq!(event.payload["authMethod"], serde_json::json!("xaa"));
    assert_eq!(
        event.payload["xaaFailureStage"],
        serde_json::json!("discovery")
    );
    assert_eq!(event.payload["idTokenCacheHit"], serde_json::json!(true));
}
