mod output_schema_dispatch {
    use super::*;
    use lingxi_core::host::json_schema_validator::{
        CompiledJsonSchema, JsonSchemaValidation, JsonSchemaValidator, JsonSchemaValidatorSession,
        SchemaCompileError, SchemaProfile, SchemaWorkerError,
    };
    struct Validator;
    struct Session;
    struct Plan(String);
    impl JsonSchemaValidator for Validator {
        fn new_session(&self, _: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
            Arc::new(Session)
        }
    }
    #[async_trait]
    impl JsonSchemaValidatorSession for Session {
        async fn compile(
            &self,
            schema: &UJP,
        ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
            Ok(Arc::new(Plan(
                schema
                    .string_at("/required/0")
                    .expect("required fixture")
                    .to_owned(),
            )))
        }
    }
    #[async_trait]
    impl CompiledJsonSchema for Plan {
        async fn validate(&self, output: &UJP) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            Ok(if output.get_property(&self.0).is_some() {
                JsonSchemaValidation::Valid
            } else {
                JsonSchemaValidation::Invalid {
                    detail: format!("required {}", self.0).into(),
                    keywords: vec!["required".into()],
                }
            })
        }
    }
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    struct CatalogTransport {
        connection: Arc<Connection>,
        tools: Vec<McpToolDto>,
        dials: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl McpTransport for CatalogTransport {
        async fn connect(&self, _: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            self.dials.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(McpRawConnection {
                connection_id: lingxi_core::types::McpConnectionId::new(),
            })
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
            Ok(self.tools.clone())
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
            _: UJP,
        ) -> Result<lingxi_core::host::McpToolResultDto, McpError> {
            unreachable!()
        }
        async fn read_resource(
            &self,
            _: &McpRawConnection,
            _: &UJP,
        ) -> Result<McpResourceContentDto, McpError> {
            unreachable!()
        }
        async fn ping(&self, _: lingxi_core::types::McpConnectionId) -> Result<(), McpError> {
            Ok(())
        }
        async fn notifications(
            &self,
            _: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!()
        }
        async fn handle_elicitation(
            &self,
            _: &McpRawConnection,
            _: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            unreachable!()
        }
        async fn disconnect(&self, _: lingxi_core::types::McpConnectionId) -> Result<(), McpError> {
            Ok(())
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Http]
        }
    }
    impl mcp::RawConnectionProvider for CatalogTransport {
        fn connection_for(
            &self,
            _: lingxi_core::types::McpConnectionId,
        ) -> Option<Arc<Connection>> {
            Some(self.connection.clone())
        }
    }

    struct CacheEnv(Option<std::ffi::OsString>);
    impl Drop for CacheEnv {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("MCP_DISCOVERY_CACHE", value),
                None => std::env::remove_var("MCP_DISCOVERY_CACHE"),
            }
        }
    }

    async fn registered_tools(
        registry: &Arc<McpRegistry>,
        raw: Option<&str>,
    ) -> tool_api::registry::ToolRegistry {
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![std::env::temp_dir()],
        );
        ctx.mcp_registry = Some(registry.clone());
        let tools = tool_api::registry::ToolRegistry::new();
        let bound = build_registered_mcp_tools(registry, ctx).await;
        tools.replace_mcp_tools(
            bound
                .into_iter()
                .map(|(connection, tools)| {
                    (
                        connection,
                        tools
                            .into_iter()
                            .filter(|tool| {
                                raw.is_none_or(|raw| {
                                    tool.mcp_tool_identity()
                                        .is_some_and(|identity| identity.raw_tool_name == raw)
                                })
                            })
                            .collect(),
                    )
                })
                .collect(),
        );
        tools
    }

    #[tokio::test]
    async fn ambiguous_names_are_hidden_and_exact_selected_schema_survives_live_and_cached_snapshots(
    ) {
        let _env = CacheEnv(std::env::var_os("MCP_DISCOVERY_CACHE"));
        std::env::set_var("MCP_DISCOVERY_CACHE", "true");
        let directory = tempfile::tempdir().unwrap();
        let (connection, tx, mut rx) = paired();
        let transport = Arc::new(CatalogTransport {
            connection,
            tools: ["a.b", "a_b"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| McpToolDto {
                    full_name: format!("mcp____{name}"),
                    server_name: String::new(),
                    tool_name: name.into(),
                    wire_tool_name: None,
                    description: "fixture".into(),
                    input_schema: source_fixture(&json!({"type":"object"})),
                    output_schema: (index == 0)
                        .then(|| source_fixture(&json!({"required":["ok"]}))),
                    annotations: None,
                    icons: vec![],
                    meta: None,
                    search_hint: None,
                    always_load: Some(true),
                    requires_user_interaction: false,
                })
                .collect(),
            dials: std::sync::atomic::AtomicUsize::new(0),
        });
        let config = mcp::McpServerConfig {
            name: "srv".into(),
            spec: McpTransportSpec::Http {
                url: "https://mcp.example.test/v1".into(),
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
            scope: mcp::ConfigScope::Dynamic,
            disabled: false,
            timeout_ms: None,
            always_load: true,
            discovery_cache: Some(true),
            tools: vec![],
            tool_permissions: Default::default(),
            config_error: None,
            metadata: Default::default(),
        };
        let store = mcp::DiscoveryCacheStore::new(directory.path());
        let make_registry = || {
            Arc::new(
                McpRegistry::with_raw_conn(transport.clone(), transport.clone())
                    .with_json_schema_validator(Arc::new(Validator))
                    .with_discovery_cache_store(store.clone()),
            )
        };
        let live = make_registry();
        live.connect(config.clone()).await.unwrap();
        let ambiguous = registered_tools(&live, None).await;
        assert!(ambiguous.find_by_name("mcp__srv__a_b").is_none());
        assert!(!ambiguous.all_names().contains(&"mcp__srv__a_b".to_owned()));
        let live_tools = registered_tools(&live, Some("a.b")).await;
        let selected = live_tools.find_by_name("mcp__srv__a_b").unwrap();
        assert_eq!(selected.mcp_tool_identity().unwrap().raw_tool_name, "a.b");
        let bad = source_fixture(&json!({"bad":true}));
        assert!(selected
            .output_parser_snapshot()
            .await
            .unwrap()
            .unwrap()
            .parse_output(&bad)
            .await
            .is_err());
        let call = selected.call_with_output_parser(
            source_fixture(&json!({})),
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
            lingxi_core::host::tool_invoker::ToolOutputParserCapture::default(),
        );
        let respond = async {
            let request = request(&mut rx, "tools/call").await;
            assert_eq!(request["params"]["name"], "a.b");
            reply(
                &tx,
                &request,
                json!({"content":[],"structuredContent":{"bad":true}}),
            )
            .await;
        };
        let (result, ()) = tokio::join!(call, respond);
        assert!(result
            .as_ref()
            .map(|result| result.is_error)
            .unwrap_or(true));
        let cached = make_registry();
        cached.connect(config).await.unwrap();
        assert!(matches!(
            cached.connections.read().await.get("srv"),
            Some(mcp::McpConnectionState::Cached { .. })
        ));
        let ambiguous = registered_tools(&cached, None).await;
        assert!(ambiguous.find_by_name("mcp__srv__a_b").is_none());
        assert!(!ambiguous.all_names().contains(&"mcp__srv__a_b".to_owned()));
        let cached_tools = registered_tools(&cached, Some("a.b")).await;
        let selected = cached_tools.find_by_name("mcp__srv__a_b").unwrap();
        assert_eq!(selected.mcp_tool_identity().unwrap().raw_tool_name, "a.b");
        assert!(selected
            .output_parser_snapshot()
            .await
            .unwrap()
            .unwrap()
            .parse_output(&bad)
            .await
            .is_err());
        assert_eq!(
            transport.dials.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Cached Mod-only validation must remain local"
        );
    }

    async fn request(rx: &mut mpsc::Receiver<Bytes>, method: &str) -> Value {
        loop {
            let bytes = tokio::time::timeout(WAIT, rx.recv())
                .await
                .unwrap()
                .unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            if request.get("id").is_none() {
                continue;
            }
            assert_eq!(request["method"], method);
            return request;
        }
    }
    async fn reply(tx: &mpsc::Sender<Bytes>, req: &Value, result: Value) {
        let response = json!({"jsonrpc":"2.0", "id":req["id"], "result":result});
        tx.send(Bytes::from(format!("{response}\n"))).await.unwrap();
    }
    async fn list(
        client: &Arc<mcp::McpClient>,
        tx: &mpsc::Sender<Bytes>,
        rx: &mut mpsc::Receiver<Bytes>,
        field: &str,
    ) {
        let task = {
            let client = client.clone();
            tokio::spawn(async move { client.list_tools().await })
        };
        let req = request(rx, "tools/list").await;
        reply(tx, &req, json!({"tools":[{"name":"slowtool", "inputSchema":{"type":"object"}, "outputSchema":{"required":[field]}}]})).await;
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn mcp_tool_hands_mod_the_physical_rpc_schema_instead_of_bound_or_refreshed_schema() {
        let (conn, tx, mut rx) = paired();
        let client = Arc::new(
            mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn)
                .await
                .with_json_schema_validator(Arc::new(Validator)),
        );
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client.clone()).await;
        list(&client, &tx, &mut rx, "live").await;
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::env::temp_dir()],
        );
        ctx.mcp_registry = Some(registry);
        let tool = Arc::new(MCPTool::new_for_tool(
            ctx,
            "mcp__slow__slowtool".into(),
            "fixture".into(),
            source_fixture(&json!({"type":"object"})),
            Some(source_fixture(&json!({"required":["cached"]}))),
            None,
            None,
            false,
            false,
        ));
        let capture = lingxi_core::host::tool_invoker::ToolOutputParserCapture::default();
        let task = {
            let tool = tool.clone();
            let capture = capture.clone();
            tokio::spawn(async move {
                tool.call_with_output_parser(
                    source_fixture(&json!({})),
                    tool_api::test_support::fresh_ctx(),
                    tool_api::test_support::fresh_tx(),
                    capture,
                )
                .await
            })
        };
        let req = request(&mut rx, "tools/call").await;
        list(&client, &tx, &mut rx, "refreshed").await;
        reply(
            &tx,
            &req,
            json!({"content":[], "structuredContent":{"live":true}}),
        )
        .await;
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .expect("ordinary RPC validates with live dispatch schema");
        let parser = capture
            .snapshot()
            .expect("physical call publishes an immutable parser");
        parser
            .parse_output(&source_fixture(&json!({"live":true})))
            .await
            .expect("Mod replacement retains physical call contract");
        assert!(parser
            .parse_output(&source_fixture(&json!({"cached":true})))
            .await
            .is_err());
        assert!(parser
            .parse_output(&source_fixture(&json!({"refreshed":true})))
            .await
            .is_err());
        assert!(
            tool.parse_output(&source_fixture(&json!({"live":true})))
                .await
                .is_err(),
            "current catalog has moved on"
        );
    }
}
