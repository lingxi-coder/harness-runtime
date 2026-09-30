use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError,
    McpNotificationStream, McpRawConnection, McpResourceContentDto, McpResourceDto,
    McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use std::sync::Arc;
use std::time::Duration;

/// Its `connect` never resolves, so the connect loop's SECOND iteration
/// parks forever inside `connect_agent_scoped` — at which point the FIRST
/// server is already live and its cleanup handle already sits in the
/// function's local `cleanups` vec. Dropping the future there is exactly
/// the Fusion `join_set.abort_all()` race the finding describes.
struct HangingConnectTransport {
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl McpTransport for HangingConnectTransport {
    async fn connect(&self, _spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        self.entered.notify_one();
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves")
    }

    async fn connect_and_initialize(
        &self,
        _spec: &McpTransportSpec,
        _options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        // Overridden: the trait default wraps `connect` in a DEADLINE, and
        // a deadline would let the loop move on instead of parking.
        self.entered.notify_one();
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves")
    }

    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        unreachable!("connect never resolves")
    }

    async fn list_tools(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpToolDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_prompts(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpPromptDto>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        _tool: &str,
        _input: serde_json::Value,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!("unused")
    }

    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!("unused")
    }

    async fn ping(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        Err(McpError::Connection("unused".into()))
    }

    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Ok(ElicitResultDto {
            data: serde_json::json!({ "action": "cancel" }),
        })
    }

    async fn disconnect(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio, McpTransportKind::Http]
    }
}

fn record_spec(name: &str) -> agent::AgentMcpServerSpec {
    let mut server = serde_json::Map::new();
    server.insert(
        name.into(),
        serde_json::json!({ "command": "unused-in-test" }),
    );
    agent::AgentMcpServerSpec::Record(server)
}

async fn connect_loop_fixture() -> (
    lingxi_core::types::AgentId,
    String,
    Arc<mcp::McpRegistry>,
    tool_api::BuiltinToolContext,
    agent::AgentDefinition,
    Arc<tokio::sync::Notify>,
) {
    let agent_id = lingxi_core::types::AgentId::new();
    let opened_key = mcp::registry::agent_scope_table_key(agent_id, "opened");
    let entered = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(mcp::McpRegistry::new(Arc::new(HangingConnectTransport {
        entered: entered.clone(),
    })));
    // `opened` is already live, so `connect_agent_scoped` short-circuits
    // on it and the loop pushes its cleanup handle; `hangs` is not, so
    // its connect parks in the transport forever.
    let config = mcp::build_server_from_json_entry(
        "opened",
        &serde_json::json!({ "command": "unused-in-test" }),
        mcp::ConfigScope::Agent,
    )
    .expect("agent MCP config parses");
    registry.connections.write().await.insert(
        opened_key.clone(),
        mcp::McpConnectionState::Connected {
            config,
            connection_id: lingxi_core::types::McpConnectionId::new(),
            capabilities: lingxi_core::host::ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                logging: false,
                directory_read: false,
                experimental: std::collections::HashMap::new(),
                extensions: std::collections::HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            connected_at: std::time::SystemTime::now(),
        },
    );

    let mut def = agent::parse_agent_from_json(
        "tester",
        &serde_json::json!({"description": "d", "prompt": "p"}),
        agent::AgentSource::Settings(lingxi_core::types::SettingsScope::Project),
    )
    .expect("agent definition parses");
    def.mcp_servers = vec![record_spec("opened"), record_spec("hangs")];

    let mut ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![std::path::PathBuf::from("/tmp")],
    );
    ctx.mcp_registry = Some(registry.clone());

    (agent_id, opened_key, registry, ctx, def, entered)
}

#[tokio::test]
async fn a_dropped_connect_loop_tears_down_the_servers_it_already_opened() {
    let (agent_id, opened_key, registry, ctx, def, _) = connect_loop_fixture().await;
    let outcome = tokio::time::timeout(
        Duration::from_millis(300),
        super::build_agent_mcp_tool_set(registry.clone(), ctx, false, false, agent_id, def, None),
    )
    .await;
    assert!(
        outcome.is_err(),
        "sanity: the second server's connect must still be parked when the \
future is dropped — otherwise this test is not exercising the drop window"
    );

    // The guard's teardown is spawned onto the runtime, exactly like
    // `SpawnDeallocGuard`'s; give it a bounded window to land.
    let mut still_connected = true;
    for _ in 0..200 {
        if !registry.connections.read().await.contains_key(&opened_key) {
            still_connected = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !still_connected,
        "the MCP server the connect loop had ALREADY opened ({opened_key}) must be \
disconnected when the future is dropped mid-loop — the half-built `cleanups` vec is \
a plain local that no caller has ever seen, so nothing else can ever tear it down"
    );
}

#[tokio::test]
async fn restored_identity_stays_reserved_until_cancelled_connect_loop_cleanup_finishes() {
    use agent::StreamingSubagentSpawner;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct NoWork;
    #[async_trait::async_trait]
    impl lingxi_core::host::ToolInvoker for NoWork {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn invoke(
            &self,
            _: &str,
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
            Ok(serde_json::Value::Null)
        }
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::BudgetEnforcerHandle for NoWork {
        async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::subagent_spawn::SubagentSpawnObserver for NoWork {
        async fn on_event(&self, _: lingxi_core::host::subagent_spawn::SubagentObservation) {}
    }
    struct LeaseProbe {
        _lease: Option<agent::agent_mcp_tools::AgentMcpConstructionLease>,
        released: Arc<AtomicBool>,
    }
    impl Drop for LeaseProbe {
        fn drop(&mut self) {
            drop(self._lease.take());
            self.released.store(true, Ordering::SeqCst);
        }
    }
    let (id, opened_key, registry, ctx, definition, entered) = connect_loop_fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    let build_calls = calls.clone();
    let release_probe = released.clone();
    let build_registry = registry.clone();
    let builder: agent::agent_mcp_tools::AgentMcpToolBuilder = Arc::new(move |actual, _, lease| {
        assert_eq!(actual, id);
        build_calls.fetch_add(1, Ordering::SeqCst);
        let lease = Arc::new(LeaseProbe {
            _lease: Some(lease.expect("restored construction must carry its identity reservation")),
            released: release_probe.clone(),
        }) as agent::agent_mcp_tools::AgentMcpConstructionLease;
        Box::pin(super::build_agent_mcp_tool_set(
            build_registry.clone(),
            ctx.clone(),
            false,
            false,
            actual,
            definition.clone(),
            Some(lease),
        ))
    });
    let pool = Arc::new(agent::StateMachinePool::new(
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        2,
    ));
    let spawner = Arc::new(agent::PoolSubagentSpawner::new(pool).with_mcp_tool_builder(builder));
    let request = lingxi_core::host::SubagentSpawnRequest {
        subagent_type: "general-purpose".into(),
        resumed_history: Some(vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "restored history".into(),
        )]),
        ..Default::default()
    };
    let inherit = || lingxi_core::host::SubagentInheritance {
        tool_invoker: Arc::new(NoWork),
        budget: Arc::new(NoWork),
    };
    let worker_spawner = spawner.clone();
    let worker_request = request.clone();
    let worker_inherit = inherit();
    let worker = tokio::spawn(async move {
        worker_spawner
            .restore_persistent_with_observer(id, worker_request, worker_inherit, Arc::new(NoWork))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    // The first connection has a cleanup receipt; the second dial is held.
    // Block that real cleanup, then cancel the builder before it returns.
    let connection_gate = registry.connections.write().await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(
        !released.load(Ordering::SeqCst),
        "the construction lease must move into the asynchronous cleanup"
    );
    let collision = tokio::time::timeout(
        Duration::from_secs(1),
        spawner.restore_persistent_with_observer(id, request.clone(), inherit(), Arc::new(NoWork)),
    )
    .await;
    assert!(
        matches!(collision, Ok(Err(_))),
        "retry must fail before entering the builder while old cleanup is pending"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(connection_gate);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !released.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!registry.connections.read().await.contains_key(&opened_key));
    let retry_spawner = spawner.clone();
    let retry_inherit = inherit();
    let retry = tokio::spawn(async move {
        retry_spawner
            .restore_persistent_with_observer(id, request, retry_inherit, Arc::new(NoWork))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "identity must become reusable after cleanup, not leak forever"
    );
    retry.abort();
    let _ = retry.await;
}
