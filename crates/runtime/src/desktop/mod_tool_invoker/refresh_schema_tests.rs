// Exercise the real refresh -> selected Tool -> Mod fallback path before the
// host replaces its model catalog, using both public orchestrator drivers.
mod removed_raw_catalog_row {
    use super::*;
    use ::mcp::{ConfigScope, McpRegistry, McpServerConfig, RawConnectionProvider};
    use async_trait::async_trait;
    use bytes::Bytes;
    use lingxi_core::host::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
    use lingxi_core::types::{AgentId, McpConnectionId};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::sync::mpsc;

    fn parse(text: &str) -> Source {
        Source::parse(text).unwrap()
    }
    #[derive(Default)]
    struct Transport {
        dials: AtomicUsize,
        version: Arc<AtomicUsize>,
        rpc_count: Arc<AtomicUsize>,
        list_rpcs: Arc<AtomicUsize>,
        disconnects: AtomicUsize,
        conns: Mutex<HashMap<McpConnectionId, Arc<jsonrpc::Connection>>>,
    }
    #[async_trait]
    impl McpTransport for Transport {
        async fn connect(&self, _: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let id = McpConnectionId::new();
            let (to_client_tx, to_client_rx) = mpsc::channel::<Bytes>(8);
            let (to_peer_tx, mut to_peer_rx) = mpsc::channel::<Bytes>(8);
            let connection = Arc::new(jsonrpc::Connection::new_streams(
                to_client_rx,
                to_peer_tx,
                jsonrpc::Mode::Lines,
            ));
            self.conns.lock().unwrap().insert(id, connection);
            let count = self.rpc_count.clone();
            let version = self.version.clone();
            let lists = self.list_rpcs.clone();
            tokio::spawn(async move {
                while let Some(frame) = to_peer_rx.recv().await {
                    let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                    if request.get("id").is_none() {
                        continue;
                    }
                    let result = match request["method"].as_str().unwrap() {
                        "tools/list" => {
                            lists.fetch_add(1, Ordering::SeqCst);
                            assert_eq!(version.load(Ordering::SeqCst), 1);
                            serde_json::json!({"tools":[{"name":"a_b","inputSchema":{"type":"object"}}]})
                        }
                        "tools/call" => {
                            count.fetch_add(1, Ordering::SeqCst);
                            assert_eq!(request["params"]["name"], "a.b");
                            serde_json::json!({"content":[],"structuredContent":{"bad":true}})
                        }
                        other => panic!("unexpected request {other}"),
                    };
                    let reply =
                        serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result});
                    if to_client_tx
                        .send(Bytes::from(format!("{reply}\n")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            Ok(McpRawConnection { connection_id: id })
        }
        async fn initialize(
            &self,
            _: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            Ok(ServerCapabilitiesDto {
                tools: true,
                ..Default::default()
            })
        }
        async fn list_tools(&self, _: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            Ok(vec![McpToolDto {
                full_name: "mcp__srv__a_b".into(),
                server_name: "srv".into(),
                tool_name: "a.b".into(),
                wire_tool_name: None,
                description: "fixture".into(),
                input_schema: parse("{}"),
                output_schema: Some(parse(r#"{"type":"object","required":["ok"]}"#)),
                annotations: None,
                icons: vec![],
                meta: None,
                search_hint: None,
                always_load: Some(true),
                requires_user_interaction: false,
            }])
        }
        async fn list_resources(
            &self,
            _: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            Ok(vec![])
        }
        async fn list_prompts(&self, _: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _: &McpRawConnection,
            _: &str,
            _: Source,
        ) -> Result<McpToolResultDto, McpError> {
            panic!("physical JSON-RPC must be used")
        }
        async fn read_resource(
            &self,
            _: &McpRawConnection,
            _: &Source,
        ) -> Result<McpResourceContentDto, McpError> {
            panic!("unused")
        }
        async fn ping(&self, _: McpConnectionId) -> Result<(), McpError> {
            Ok(())
        }
        async fn notifications(
            &self,
            _: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            panic!("unused")
        }
        async fn handle_elicitation(
            &self,
            _: &McpRawConnection,
            _: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            panic!("unused")
        }
        async fn disconnect(&self, id: McpConnectionId) -> Result<(), McpError> {
            self.disconnects.fetch_add(1, Ordering::SeqCst);
            self.conns.lock().unwrap().remove(&id);
            Ok(())
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Http]
        }
    }
    impl RawConnectionProvider for Transport {
        fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
            self.conns.lock().unwrap().get(&id).cloned()
        }
    }

    async fn registered(
        registry: &Arc<McpRegistry>,
        cwd: &std::path::Path,
    ) -> Arc<tool_api::ToolRegistry> {
        let mut context = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![cwd.to_owned()],
        );
        context.mcp_registry = Some(registry.clone());
        let tools = Arc::new(tool_api::ToolRegistry::new());
        tools.replace_mcp_tools(tool_mcp::build_registered_mcp_tools(registry, context).await);
        tools
    }

    async fn check_removed_raw_row(streaming: bool) {
        use ::orchestrator::test_support::*;
        use ::orchestrator::test_support_stream::*;
        use lingxi_core::types::{ContentBlock as Block, ToolUseId};
        let directory = tempfile::tempdir().unwrap();
        let transport = Arc::new(Transport::default());
        let registry = Arc::new(
            McpRegistry::with_raw_conn(transport.clone(), transport.clone())
                .with_json_schema_validator(
                    crate::json_schema_validator::BunJsonSchemaValidator::new_from_host_environment(
                    ),
                ),
        );
        let config = McpServerConfig {
            name: "srv".into(),
            spec: McpTransportSpec::Http {
                url: "https://mcp.example.invalid/v1".into(),
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
            scope: ConfigScope::Dynamic,
            disabled: false,
            timeout_ms: None,
            always_load: true,
            discovery_cache: Some(false),
            tools: vec![],
            tool_permissions: Default::default(),
            config_error: None,
            metadata: Default::default(),
        };
        let connection_id = registry.connect(config).await.unwrap();
        let tools = registered(&registry, directory.path()).await;
        let full = "mcp__srv__a_b";
        let selected = tools.find_by_name(full).unwrap();
        assert_eq!(selected.mcp_tool_identity().unwrap().raw_tool_name, "a.b");
        assert!(selected.output_schema().is_some());
        let original = selected.output_parser_snapshot().await.unwrap().unwrap();
        let bad = parse(r#"{"bad":true}"#);
        assert!(original.parse_output(&bad).await.is_err());
        transport.version.store(1, Ordering::SeqCst);
        registry
            .refresh_catalog(&::mcp::McpCatalogChanged {
                server_name: "srv".into(),
                connection_id,
                retired_connection_id: None,
                kind: ::mcp::McpCatalogKind::Tools,
                telemetry_cause: None,
            })
            .await
            .unwrap();
        original
            .parse_output(&parse(r#"{"ok":true}"#))
            .await
            .unwrap();
        assert!(
            original.parse_output(&bad).await.is_err(),
            "in-flight plan remains immutable"
        );
        assert!(selected
            .output_parser_snapshot()
            .await
            .unwrap()
            .unwrap()
            .parse_output(&bad)
            .await
            .is_err());

        let (mod_fixture, _, _session) = setup(
            r#"export function register(on) { on('tool.call', async ($, e) => ({result:{bad:true}})); }"#,
        ).await;
        let child = ModSubagentToolInvoker::new(
            Arc::new(tool_api::tool_invoker_impl::RegistryToolInvoker::new(
                tools.clone(),
            )),
            mod_fixture.hooks.clone(),
        );
        let child_result = child
            .invoke_detailed(full, parse("{}"), child_context(AgentId::new()))
            .await
            .unwrap();
        assert!(
            child_result.is_error,
            "a removed raw row is not schema absence in a child no-next result"
        );
        assert!(
            selected
                .call(
                    parse("{}"),
                    tool_api::test_support::fresh_ctx(),
                    tool_api::test_support::fresh_tx()
                )
                .await
                .is_err(),
            "removed row fails before a physical RPC"
        );

        let id = ToolUseId::new();
        let api = Arc::new(MockApiClient::new(vec![
            mock_message_response(
                vec![llm_runtime::ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: full.into(),
                    input: llm_runtime::services::sdk::protocol::JsonPayload::parse("{}").unwrap(),
                }],
                Some("tool_use"),
            ),
            mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            ),
        ]));
        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![
            vec![
                message_start("removed-row-call", "claude-opus-4-7"),
                content_block_start_tool_use(0, id.clone(), full),
                input_json_delta(0, "{}"),
                content_block_stop(0),
                message_delta_stop("tool_use"),
                message_stop(),
            ],
            vec![
                message_start("removed-row-done", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ],
        ]));
        let orch = ::orchestrator::ConversationOrchestrator::into_shared(
            ::orchestrator::ConversationOrchestrator::new_with_streaming(
                ::orchestrator::OrchestratorConfig {
                    model: "claude-opus-4-7".into(),
                    ..Default::default()
                },
                api,
                stream,
                tools.clone(),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                directory.path().to_owned(),
            )
            .with_hook_registry(mod_fixture.hooks.clone())
            .with_mcp_registry(registry.clone()),
        );
        if streaming {
            orch.run_turn_streaming("removed raw row").await.unwrap();
        } else {
            orch.run_turn("removed raw row").await.unwrap();
        }
        let history = orch.snapshot_history().await;
        let outcome = history
            .iter()
            .flat_map(|row| match row {
                lingxi_core::types::ConversationMessage::User { content, .. }
                | lingxi_core::types::ConversationMessage::Assistant { content, .. } => {
                    content.as_slice()
                }
                _ => &[],
            })
            .find_map(|block| match block {
                Block::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } if tool_use_id == &id => Some(*is_error == Some(true)),
                _ => None,
            });
        assert_eq!(outcome, Some(true), "main driver streaming={streaming}");
        let refreshed = registered(&registry, directory.path()).await;
        let plain = refreshed.find_by_name(full).unwrap();
        assert!(plain.output_schema().is_none());
        plain
            .output_parser_snapshot()
            .await
            .unwrap()
            .unwrap()
            .parse_output(&bad)
            .await
            .unwrap();
        assert_eq!(transport.dials.load(Ordering::SeqCst), 1);
        assert_eq!(transport.list_rpcs.load(Ordering::SeqCst), 1);
        assert_eq!(
            transport.rpc_count.load(Ordering::SeqCst),
            0,
            "no-next and removed raw calls stay local"
        );
        registry.disconnect("srv").await.unwrap();
    }

    #[tokio::test]
    async fn batched_removed_raw_catalog_row_cannot_cancel_selected_output_contract() {
        check_removed_raw_row(false).await;
    }
    #[tokio::test]
    async fn streaming_removed_raw_catalog_row_cannot_cancel_selected_output_contract() {
        check_removed_raw_row(true).await;
    }
}
