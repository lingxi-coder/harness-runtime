mod cached_catalog {
    use super::*;
    use lingxi_core::host::{
        ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
        McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
        McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
    };
    use lingxi_core::types::McpConnectionId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Transport {
        base: test_harness::mocks::MockMcpTransport,
        tools: Vec<McpToolDto>,
        connects: AtomicUsize,
    }

    #[async_trait]
    impl McpTransport for Transport {
        async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            self.base.connect(spec).await
        }
        async fn initialize(
            &self,
            conn: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            self.base.initialize(conn).await
        }
        async fn list_tools(&self, _: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            Ok(self.tools.clone())
        }
        async fn list_resources(
            &self,
            conn: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            self.base.list_resources(conn).await
        }
        async fn list_prompts(
            &self,
            conn: &McpRawConnection,
        ) -> Result<Vec<McpPromptDto>, McpError> {
            self.base.list_prompts(conn).await
        }
        async fn call_tool(
            &self,
            conn: &McpRawConnection,
            name: &str,
            input: Utf16JsonProjection,
        ) -> Result<McpToolResultDto, McpError> {
            self.base.call_tool(conn, name, input).await
        }
        async fn read_resource(
            &self,
            conn: &McpRawConnection,
            uri: &Utf16JsonProjection,
        ) -> Result<McpResourceContentDto, McpError> {
            self.base.read_resource(conn, uri).await
        }
        async fn ping(&self, id: McpConnectionId) -> Result<(), McpError> {
            self.base.ping(id).await
        }
        async fn notifications(
            &self,
            conn: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            self.base.notifications(conn).await
        }
        async fn handle_elicitation(
            &self,
            conn: &McpRawConnection,
            request: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            self.base.handle_elicitation(conn, request).await
        }
        async fn disconnect(&self, id: McpConnectionId) -> Result<(), McpError> {
            self.base.disconnect(id).await
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Http]
        }
    }

    impl mcp::RawConnectionProvider for Transport {
        fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
            mcp::RawConnectionProvider::connection_for(&self.base, id)
        }
    }

    struct CacheEnv(Option<std::ffi::OsString>);
    impl Drop for CacheEnv {
        fn drop(&mut self) {
            if let Some(value) = self.0.take() {
                std::env::set_var("MCP_DISCOVERY_CACHE", value);
            } else {
                std::env::remove_var("MCP_DISCOVERY_CACHE");
            }
        }
    }

    async fn check_cached_mod_catalog(streaming: bool, without_validator: bool) {
        use lingxi_core::types::{ContentBlock as Block, ToolUseId};
        use orchestrator::test_support::*;
        let _env = CacheEnv(std::env::var_os("MCP_DISCOVERY_CACHE"));
        std::env::set_var("MCP_DISCOVERY_CACHE", "true");
        let dir = tempfile::tempdir().unwrap();
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
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: Default::default(),
        };
        let schemas = [
            json!({"$id":"https://schema.example/row","type":"object","required":["old"]}),
            json!({"$ref":"https://schema.example/row"}),
            json!({"$id":"https://schema.example/row","type":"object","required":["new"]}),
        ];
        let mut tools: Vec<McpToolDto> = ["base", "referenced", "duplicate"]
            .into_iter()
            .zip(schemas)
            .map(|(name, schema)| McpToolDto {
                full_name: format!("mcp__srv__{name}"),
                server_name: "srv".into(),
                tool_name: name.into(),
                wire_tool_name: None,
                description: "cached catalog fixture".into(),
                input_schema: source(&json!({"type":"object"})),
                output_schema: Some(source(&schema)),
                annotations: None,
                icons: Vec::new(),
                meta: None,
                search_hint: None,
                always_load: Some(true),
                requires_user_interaction: false,
            })
            .collect();
        let mut plain = tools[0].clone();
        plain.full_name = "mcp__srv__plain".into();
        plain.tool_name = "plain".into();
        plain.output_schema = None;
        tools.push(plain);
        let transport = Arc::new(Transport {
            base: test_harness::mocks::MockMcpTransport::with_call_responder(),
            tools,
            connects: AtomicUsize::new(0),
        });
        let cache = mcp::discovery_cache::DiscoveryCacheStore::new(dir.path().join("cache"));
        let validator =
            crate::json_schema_validator::BunJsonSchemaValidator::new_from_host_environment();
        // Populate the identity-partitioned store through the real live path.
        let warm = mcp::McpRegistry::with_raw_conn(transport.clone(), transport.clone())
            .with_json_schema_validator(validator.clone())
            .with_discovery_cache_store(cache.clone());
        warm.connect(config.clone()).await.unwrap();
        for name in ["referenced", "duplicate"] {
            warm.output_schema_parser_handle("srv", &format!("mcp__srv__{name}"))
                .parse_output(&source(&json!({"old":true})))
                .await
                .unwrap();
        }
        let registry = mcp::McpRegistry::with_raw_conn(transport.clone(), transport.clone())
            .with_discovery_cache_store(cache);
        let registry = Arc::new(if without_validator {
            registry
        } else {
            registry.with_json_schema_validator(validator)
        });
        registry.connect(config).await.unwrap();
        assert_eq!(
            transport.connects.load(Ordering::SeqCst),
            1,
            "second owner must admit the fresh Cached catalog without dialing"
        );
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
            dir.path().to_owned(),
        ));
        let mut context = tool_api::test_support::ctx_for_file_tools(
            fs,
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![dir.path().to_owned()],
        );
        context.mcp_registry = Some(registry.clone());
        let tools = Arc::new(tool_api::registry::ToolRegistry::new());
        tools.replace_mcp_tools(tool_mcp::build_registered_mcp_tools(&registry, context).await);
        let (mod_fixture, _, _session) = setup(
            r#"export function register(on) {
          on('tool.call', async ($, e) => ({ result: e.bad ? { new: true } : { old: true } }));
        }"#,
        )
        .await;
        let child = ModSubagentToolInvoker::new(
            Arc::new(tool_api::tool_invoker_impl::RegistryToolInvoker::new(
                tools.clone(),
            )),
            mod_fixture.hooks.clone(),
        );
        let names: &[&str] = if without_validator {
            &["plain"]
        } else {
            &["referenced", "duplicate", "plain"]
        };
        for &name in names {
            let full = format!("mcp__srv__{name}");
            for bad in [false, true] {
                let result = child
                    .invoke_detailed(
                        &full,
                        source(&json!({"bad":bad})),
                        child_context(AgentId::new()),
                    )
                    .await
                    .unwrap();
                let expected_error = bad && name != "plain";
                assert_eq!(result.is_error, expected_error, "child {name} bad={bad}");
                let id = ToolUseId::new();
                let api = Arc::new(MockApiClient::new(vec![
                    mock_message_response(
                        vec![llm_runtime::ContentBlock::ToolCall {
                            id: id.to_string(),
                            name: full.clone(),
                            input: llm_runtime::services::sdk::protocol::JsonPayload::parse(
                                &format!("{{\"bad\":{bad}}}"),
                            )
                            .unwrap(),
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
                        message_start("cached-call", "claude-opus-4-7"),
                        content_block_start_tool_use(0, id.clone(), &full),
                        input_json_delta(0, &format!("{{\"bad\":{bad}}}")),
                        content_block_stop(0),
                        message_delta_stop("tool_use"),
                        message_stop(),
                    ],
                    vec![
                        message_start("cached-done", "claude-opus-4-7"),
                        content_block_start_text(0),
                        text_delta(0, "done"),
                        content_block_stop(0),
                        message_delta_stop("end_turn"),
                        message_stop(),
                    ],
                ]));
                let orch = orchestrator::ConversationOrchestrator::into_shared(
                    orchestrator::ConversationOrchestrator::new_with_streaming(
                        orchestrator::OrchestratorConfig {
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
                        dir.path().to_owned(),
                    )
                    .with_hook_registry(mod_fixture.hooks.clone())
                    .with_mcp_registry(registry.clone()),
                );
                if streaming {
                    orch.run_turn_streaming("cached replacement").await.unwrap();
                } else {
                    orch.run_turn("cached replacement").await.unwrap();
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
                assert_eq!(
                    outcome,
                    Some(expected_error),
                    "main {name} bad={bad} stream={streaming}"
                );
            }
        }
        assert_eq!(
            transport.connects.load(Ordering::SeqCst),
            1,
            "all main/child no-next replacements must stay local"
        );
        assert!(
            transport.base.called_tools().is_empty(),
            "no-next must not call the server"
        );
        registry.disconnect("srv").await.unwrap();
        warm.disconnect("srv").await.unwrap();
    }

    #[tokio::test]
    async fn batched_cached_main_and_child_mod_results_share_real_catalog_schema_semantics() {
        check_cached_mod_catalog(false, false).await;
    }
    #[tokio::test]
    async fn streaming_cached_main_and_child_mod_results_share_real_catalog_schema_semantics() {
        check_cached_mod_catalog(true, false).await;
    }
    #[tokio::test]
    async fn batched_cached_schema_less_mod_replacement_needs_no_validator() {
        check_cached_mod_catalog(false, true).await;
    }
    #[tokio::test]
    async fn streaming_cached_schema_less_mod_replacement_needs_no_validator() {
        check_cached_mod_catalog(true, true).await;
    }
}
