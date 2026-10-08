//! The initial registry catalog already owns its normal local callback authority.
use ::mcp::{
    connection::{ConfigScope, McpServerConfig},
    hook_dispatch::{ElicitationHookOutcome, ElicitationHookRequest, HookDispatcher},
    raw_conn::RawConnectionProvider,
    registry::McpRegistry,
};
use async_trait::async_trait;
use lingxi_core::host::mcp_result::{
    drive_modern_request, JsonrpcMcpResultIo, McpInputRequiredOptions,
};
use lingxi_core::{host::*, types::McpConnectionId};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
struct Transport {
    id: McpConnectionId,
    connection: Arc<jsonrpc::Connection>,
}
impl RawConnectionProvider for Transport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
        (id == self.id).then(|| self.connection.clone())
    }
}
#[async_trait]
impl McpTransport for Transport {
    async fn connect(&self, _: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        Ok(McpRawConnection {
            connection_id: self.id,
        })
    }
    async fn initialize(&self, _: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: true,
            ..Default::default()
        })
    }
    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        _: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        Ok(McpConnectResult {
            connection: self.connect(spec).await?,
            capabilities: self
                .initialize(&McpRawConnection {
                    connection_id: self.id,
                })
                .await?,
            negotiated: McpNegotiatedProtocol {
                era: McpProtocolEra::Modern,
                version: "2026-07-28".into(),
            },
        })
    }
    async fn list_tools(&self, _: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        let value = drive_modern_request(
            &JsonrpcMcpResultIo {
                connection: self.connection.clone(),
                client_capabilities: json!({"roots":{},"elicitation":{}}),
            },
            "tools/list",
            json!({}),
            McpInputRequiredOptions::default(),
        )
        .await
        .map_err(|e| McpError::Internal(e.to_string()))?;
        value["tools"].as_array().unwrap().iter().map(|tool|serde_json::from_value(json!({"server_name":"ordinary","tool_name":tool["name"],"description":"ordinary worker","input_schema":tool["inputSchema"],"full_name":"mcp__ordinary__worker"})).map_err(|e|McpError::Internal(e.to_string()))).collect()
    }
    async fn list_resources(&self, _: &McpRawConnection) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(Vec::new())
    }
    async fn list_prompts(&self, _: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        Ok(Vec::new())
    }
    async fn call_tool(
        &self,
        _: &McpRawConnection,
        _: &str,
        _: Value,
    ) -> Result<McpToolResultDto, McpError> {
        panic!("registry must use its constructed real client")
    }
    async fn read_resource(
        &self,
        _: &McpRawConnection,
        _: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        panic!("unused")
    }
    async fn ping(&self, _: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }
    async fn notifications(&self, _: &McpRawConnection) -> Result<McpNotificationStream, McpError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
    async fn handle_elicitation(
        &self,
        _: &McpRawConnection,
        _: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        panic!("embedded request must invoke registered local handler")
    }
    async fn disconnect(&self, _: McpConnectionId) -> Result<(), McpError> {
        self.connection.close();
        Ok(())
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::InProcess]
    }
}
struct Hooks(Arc<AtomicUsize>);
#[async_trait]
impl HookDispatcher for Hooks {
    async fn dispatch_elicitation(
        &self,
        request: ElicitationHookRequest,
    ) -> ElicitationHookOutcome {
        assert_eq!(request.message, "catalog consent");
        self.0.fetch_add(1, Ordering::SeqCst);
        ElicitationHookOutcome::Respond(json!({"action":"accept","content":{}}))
    }
}
#[tokio::test]
async fn first_registry_catalog_fulfils_roots_and_hooks_before_client_publication() {
    let (client_read, mut server_write) = tokio::io::duplex(65536);
    let (server_read, client_write) = tokio::io::duplex(65536);
    let connection = Arc::new(jsonrpc::Connection::new_line_delimited(
        client_read,
        client_write,
    ));
    let frames = Arc::new(Mutex::new(Vec::new()));
    let captured = frames.clone();
    let peer = tokio::spawn(async move {
        let mut lines = BufReader::new(server_read).lines();
        let mut catalog_round = 0;
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: Value = serde_json::from_str(&line).unwrap();
            captured.lock().unwrap().push(request.clone());
            if request.get("id").is_none() {
                continue;
            }
            let result = match request["method"].as_str().unwrap() {
                "tools/list" => {
                    catalog_round += 1;
                    if catalog_round == 1 {
                        json!({"resultType":"input_required","inputRequests":{"r":{"method":"roots/list"},"e":{"method":"elicitation/create","params":{"message":"catalog consent","requestedSchema":{"type":"object","properties":{}}}}}})
                    } else {
                        assert!(
                            request["params"]["inputResponses"]["r"]["roots"]
                                .as_array()
                                .unwrap()
                                .len()
                                > 0
                        );
                        assert_eq!(request["params"]["inputResponses"]["e"]["action"], "accept");
                        json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"worker","inputSchema":{"type":"object"}}]})
                    }
                }
                "tools/call" => {
                    json!({"resultType":"complete","content":[{"type":"text","text":"tool reached"}]})
                }
                _ => json!({"resultType":"complete"}),
            };
            server_write
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let transport = Arc::new(Transport {
        id: McpConnectionId::new(),
        connection,
    });
    let hooks = Arc::new(AtomicUsize::new(0));
    let registry = McpRegistry::with_raw_conn(transport.clone(), transport)
        .with_hook_dispatcher(Some(Arc::new(Hooks(hooks.clone()))));
    registry
        .connect(McpServerConfig {
            name: "ordinary".into(),
            spec: McpTransportSpec::InProcess {
                registry_key: "ordinary".into(),
            },
            scope: ConfigScope::Dynamic,
            disabled: false,
            timeout_ms: None,
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    assert_eq!(registry.servers_with_tools().await, vec!["ordinary"]);
    let client = registry.get_client("ordinary").await.unwrap();
    assert!(!client.has_pending_elicitation());
    let result = client
        .call_tool_with_timeout("mcp__ordinary__worker", json!({}), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(result.content[0]["text"], "tool reached");
    assert_eq!(
        frames
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "tools/list")
            .count(),
        2
    );
    assert!(!frames.lock().unwrap().iter().any(|r| matches!(
        r["method"].as_str(),
        Some("roots/list" | "elicitation/create")
    )));
    registry.disconnect("ordinary").await.unwrap();
    peer.abort();
}
