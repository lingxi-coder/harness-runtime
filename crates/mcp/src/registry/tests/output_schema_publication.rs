mod output_schema_publication {
    use super::*;
    use lingxi_core::host::json_schema_validator::{
        CompiledJsonSchema, JsonSchemaValidation, JsonSchemaValidator, JsonSchemaValidatorSession,
        SchemaCompileError, SchemaProfile, SchemaWorkerError,
    };

    #[tokio::test]
    async fn old_listener_terminal_failure_waits_for_replacement_and_cannot_retire_it() {
        let mock = Arc::new(BridgeMock::new(&["worker"]));
        let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone());
        let old = registry.connect(cfg("srv")).await.unwrap();
        let lifecycle = registry.lifecycle_lock("srv");
        let held = lifecycle.lock().await;
        let mut config = cfg("srv");
        config.timeout_ms = Some(5_000);
        let replacement = registry.replace_config_atomically(config);
        tokio::pin!(replacement);
        assert!(futures_util::poll!(&mut replacement).is_pending());
        let error = McpError::Connection("old listener exhausted retries".into());
        let terminal = registry.mark_listener_generation_disconnected("srv", old, &error);
        tokio::pin!(terminal);
        let waited = futures_util::poll!(&mut terminal).is_pending();
        drop(held);
        let new = replacement.await.unwrap().unwrap();
        terminal.await;
        assert!(
            waited,
            "terminal retirement must share the generation publication lock"
        );
        assert_ne!(new, old);
        assert!(matches!(registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { connection_id, .. }) if *connection_id == new));
        let client = registry.get_client_by_key("srv").await.unwrap();
        let handle = registry.output_schema_parser_handle("srv", "mcp__srv__worker");
        assert!(Arc::ptr_eq(
            handle.slot.read().await.client.as_ref().unwrap(),
            &client
        ));
        assert_eq!(mock.disconnect_calls.load(Ordering::SeqCst), 1);
        registry
            .mark_listener_generation_disconnected("srv", new, &error)
            .await;
        assert!(registry.get_client_by_key("srv").await.is_none());
        assert!(handle.slot.read().await.client.is_none());
        assert_eq!(mock.disconnect_calls.load(Ordering::SeqCst), 2);
    }

    #[derive(Default)]
    struct CatalogValidator(AtomicUsize);
    struct CatalogSession(TestMutex<HashMap<String, String>>);
    struct CatalogPlan(String);
    impl JsonSchemaValidator for CatalogValidator {
        fn new_session(&self, profile: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
            assert_eq!(profile, SchemaProfile::McpOutputSchema2020);
            self.0.fetch_add(1, Ordering::SeqCst);
            Arc::new(CatalogSession(TestMutex::new(HashMap::new())))
        }
    }
    #[async_trait]
    impl JsonSchemaValidatorSession for CatalogSession {
        async fn compile(
            &self,
            schema: &Utf16JsonProjection,
        ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
            let mut resources = self.0.lock().unwrap();
            let required = if let Some(reference) = schema.string_at("/$ref") {
                resources.get(reference).cloned().ok_or_else(|| {
                    SchemaCompileError::InvalidSchema("missing catalog resource".into())
                })?
            } else {
                let required = schema.string_at("/required/0").unwrap().to_owned();
                if let Some(id) = schema.string_at("/$id") {
                    resources.entry(id.into()).or_insert(required).clone()
                } else {
                    required
                }
            };
            Ok(Arc::new(CatalogPlan(required)))
        }
    }
    #[async_trait]
    impl CompiledJsonSchema for CatalogPlan {
        async fn validate(
            &self,
            output: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
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

    #[tokio::test]
    async fn cached_parser_uses_complete_catalog_refs_and_first_id_wins_without_dialing() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let config = http_cfg("srv", "https://mcp.example.com/v1");
        let mut mock = BridgeMock::new(&["base", "referenced", "duplicate"]);
        for (tool, schema) in mock.tools.iter_mut().zip([
            serde_json::json!({"$id":"https://schemas.example/row", "type":"object", "required":["old"]}),
            serde_json::json!({"$ref":"https://schemas.example/row"}),
            serde_json::json!({"$id":"https://schemas.example/row", "type":"object", "required":["new"]}),
        ]) {
            tool.server_name = "srv".into();
            tool.full_name = format!("mcp__srv__{}", tool.tool_name);
            tool.output_schema = Some(source_json(&schema));
        }
        seed_entry_with_catalog(
            &store,
            &crate::discovery_cache::logical_cache_key(&config),
            0,
            ServerCapabilitiesDto {
                tools: true,
                ..Default::default()
            },
            mock.tools.clone(),
            vec![],
            vec![],
        );
        let mock = Arc::new(mock);
        let validator = Arc::new(CatalogValidator::default());
        let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
            .with_json_schema_validator(validator.clone())
            .with_discovery_cache_store(store);
        registry.connect(config).await.unwrap();
        for live in [false, true] {
            if live {
                registry.ensure_dialed_from_cache("srv").await.unwrap();
            }
            for name in ["referenced", "duplicate"] {
                let full_name = format!("mcp__srv__{name}");
                let snapshot = registry
                    .output_schema_parser_handle("srv", &full_name)
                    .snapshot(None)
                    .await
                    .unwrap();
                snapshot
                    .parse_output(&source_json(&serde_json::json!({"old":true})))
                    .await
                    .unwrap();
                assert!(
                    matches!(
                        snapshot
                            .parse_output(&source_json(&serde_json::json!({"new":true})))
                            .await,
                        Err(McpClientError::OutputSchemaInvalid { .. })
                    ),
                    "live={live}, tool={name}"
                );
            }
            assert_eq!(mock.connect_calls.load(Ordering::SeqCst), usize::from(live));
            assert_eq!(validator.0.load(Ordering::SeqCst), 1 + usize::from(live));
        }
        registry.disconnect("srv").await.unwrap();
    }
    #[derive(Default)]
    struct Validator {
        fail: AtomicBool,
        compiles: AtomicUsize,
    }
    struct Session {
        fail: bool,
    }
    struct Plan;
    impl JsonSchemaValidator for Validator {
        fn new_session(&self, _: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
            self.compiles.fetch_add(1, Ordering::SeqCst);
            Arc::new(Session {
                fail: self.fail.swap(false, Ordering::SeqCst),
            })
        }
    }
    #[async_trait]
    impl JsonSchemaValidatorSession for Session {
        async fn compile(
            &self,
            schema: &Utf16JsonProjection,
        ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
            assert_eq!(schema.string_at("/required/0"), Some("old"));
            if self.fail {
                Err(SchemaCompileError::Worker("fixture restarting".into()))
            } else {
                Ok(Arc::new(Plan))
            }
        }
    }
    #[async_trait]
    impl CompiledJsonSchema for Plan {
        async fn validate(
            &self,
            output: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            Ok(if output.get_property("old").is_some() {
                JsonSchemaValidation::Valid
            } else {
                JsonSchemaValidation::Invalid {
                    detail: "required old".into(),
                    keywords: vec!["required".into()],
                }
            })
        }
    }
    fn tool() -> McpToolDto {
        let mut tool = BridgeMock::new(&["worker"]).tools.remove(0);
        tool.full_name = "mcp__srv__worker".into();
        tool.server_name = "srv".into();
        tool.output_schema = Some(source_json(
            &serde_json::json!({"type":"object", "required":["old"]}),
        ));
        tool
    }
    async fn assert_installed(registry: &McpRegistry) {
        let client = registry
            .get_client_by_key("srv")
            .await
            .expect("published client");
        assert!(
            matches!(
                client
                    .parse_tool_output("mcp__srv__worker", &source_json(&serde_json::json!({})))
                    .await,
                Err(McpClientError::OutputSchemaInvalid { .. })
            ),
            "retained catalog's plan must precede publication"
        );
        registry
            .output_schema_parser_handle("srv", "mcp__srv__worker")
            .parse_output(&source_json(&serde_json::json!({"old":true})))
            .await
            .unwrap();
        let tools = match registry.connections.read().await.get("srv").unwrap() {
            McpConnectionState::Connected { tools, .. } => tools.clone(),
            _ => panic!("live connection expected"),
        };
        assert_eq!(tools.len(), 1);
        assert!(tools[0].output_schema.is_some());
    }

    #[tokio::test]
    async fn cached_upgrade_compiles_retained_tools_on_list_failure_in_both_modes() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        for age in [0, 1_000_000] {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
            let config = http_cfg("srv", "https://mcp.example.com/v1");
            let cache_key = crate::discovery_cache::logical_cache_key(&config);
            seed_entry_with_catalog(
                &store,
                &cache_key,
                age,
                ServerCapabilitiesDto {
                    tools: true,
                    ..Default::default()
                },
                vec![tool()],
                vec![],
                vec![],
            );
            let mock = Arc::new(BridgeMock::with_drivable_calls(&[]));
            mock.list_tools_fails.store(true, Ordering::SeqCst);
            let validator = Arc::new(Validator::default());
            let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
                .with_json_schema_validator(validator.clone())
                .with_discovery_cache_store(store);
            registry.connect(config).await.unwrap();
            if age == 0 {
                registry.ensure_dialed_from_cache("srv").await.unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if registry.get_client_by_key("srv").await.is_some() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("background cache upgrade publishes");
            }
            assert_installed(&registry).await;
            assert_eq!(validator.compiles.load(Ordering::SeqCst), 1);
            registry.disconnect("srv").await.unwrap();
        }
        drop(env);
    }

    #[tokio::test]
    async fn cached_worker_failure_restores_admitted_cache_and_next_call_retries_in_both_modes() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        for age in [0, 1_000_000] {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
            let config = http_cfg("srv", "https://mcp.example.com/v1");
            let cache_key = crate::discovery_cache::logical_cache_key(&config);
            seed_entry_with_catalog(
                &store,
                &cache_key,
                age,
                ServerCapabilitiesDto {
                    tools: true,
                    resources: true,
                    prompts: true,
                    ..Default::default()
                },
                vec![tool()],
                vec![resource("cached-resource", "file:///cached.md")],
                vec![prompt("cached-prompt")],
            );
            let mock = Arc::new(BridgeMock::with_drivable_calls(&[]));
            mock.list_tools_fails.store(true, Ordering::SeqCst);
            mock.block_list_tools.store(true, Ordering::SeqCst);
            let validator = Arc::new(Validator::default());
            validator.fail.store(true, Ordering::SeqCst);
            let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
                .with_json_schema_validator(validator.clone())
                .with_discovery_cache_store(store.clone());
            let cached_id = registry.connect(config).await.unwrap();
            let original = registry
                .connections
                .read()
                .await
                .get("srv")
                .unwrap()
                .clone();
            assert!(matches!(&original, McpConnectionState::Cached { .. }));

            // In either mode the first real call joins this upgrade while its
            // tools/list is held outside the lifecycle lock. No timing sleeps.
            let first = registry.call_tool_with_auth_retry(
                "srv",
                "mcp__srv__worker",
                source_json(&serde_json::json!({})),
                None,
                None,
            );
            tokio::pin!(first);
            futures_util::future::poll_fn(|cx| {
                assert!(first.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            tokio::time::timeout(Duration::from_secs(5), mock.list_tools_started.notified())
                .await
                .unwrap();
            mock.block_list_tools.store(false, Ordering::SeqCst);
            mock.list_tools_release.notify_one();
            let error = tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .unwrap()
                .expect_err("the first dispatch observes transient preparation failure");
            assert!(
                error.to_string().contains("fixture restarting"),
                "preserve original worker error: {error}"
            );
            let restored = registry
                .connections
                .read()
                .await
                .get("srv")
                .unwrap()
                .clone();
            assert_eq!(
                format!("{restored:?}"),
                format!("{original:?}"),
                "restore the complete admitted cache, including non-tool slices"
            );
            assert!(
                matches!(restored, McpConnectionState::Cached { connection_id, .. } if connection_id == cached_id)
            );
            assert!(registry.get_client_by_key("srv").await.is_none());
            assert!(registry.lazy_upgrade_slot("srv").await.is_none());
            assert_eq!(
                mock.disconnect_calls.load(Ordering::SeqCst),
                1,
                "discard failed prepared connection"
            );
            let entry = match load_test_entry(&store, &cache_key) {
                crate::discovery_cache::EntryLookup::Found(entry) => entry,
                other => {
                    panic!("local worker failure must preserve admitted disk cache: {other:?}")
                }
            };
            assert_eq!(
                entry.consecutive_refresh_failures, 0,
                "a local worker failure is not a server cache failure"
            );

            let second = registry.call_tool_with_auth_retry(
                "srv",
                "mcp__srv__worker",
                source_json(&serde_json::json!({})),
                None,
                None,
            );
            let respond = async {
                let live_id = loop {
                    if let Some(McpConnectionState::Connected { connection_id, .. }) =
                        registry.connections.read().await.get("srv")
                    {
                        break *connection_id;
                    }
                    tokio::task::yield_now().await;
                };
                let (tx, mut rx) = mock.take_tool_call_peer(live_id).unwrap();
                let request = loop {
                    let frame = rx.recv().await.unwrap();
                    let request: Value = serde_json::from_slice(&frame).unwrap();
                    if request.get("id").is_some() {
                        break request;
                    }
                };
                assert_eq!(request["method"], "tools/call");
                let response = serde_json::json!({"jsonrpc":"2.0", "id":request["id"],
                    "result":{"content":[], "structuredContent":{"old":true}}});
                tx.send(Bytes::from(format!("{response}\n"))).await.unwrap();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(second, respond)
            })
            .await
            .expect("next call must retry and reach a physical RPC");
            assert_eq!(
                result.unwrap().structured_content,
                Some(source_json(&serde_json::json!({"old":true})))
            );
            assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);
            assert_eq!(validator.compiles.load(Ordering::SeqCst), 2);
            assert_installed(&registry).await;
            registry.disconnect("srv").await.unwrap();
        }
        drop(env);
    }

    #[tokio::test]
    async fn listener_reopen_compiles_retained_catalog_and_rejects_worker_failure_before_publish() {
        let mut mock = BridgeMock::with_modern_drivable_calls(&["worker"]);
        mock.tools = vec![tool()];
        let mock = Arc::new(mock);
        let validator = Arc::new(Validator::default());
        let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
            .with_json_schema_validator(validator.clone());
        let initial = registry.connect(cfg("srv")).await.unwrap();
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        registry
            .reopen_catalog_listener_generation(
                "srv",
                initial,
                telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                1,
                0,
            )
            .await
            .unwrap();
        assert_installed(&registry).await;
        let current = match registry.connections.read().await.get("srv").unwrap() {
            McpConnectionState::Connected { connection_id, .. } => *connection_id,
            _ => panic!("live connection"),
        };
        let previous = registry.get_client_by_key("srv").await.unwrap();
        validator.fail.store(true, Ordering::SeqCst);
        assert!(registry
            .reopen_catalog_listener_generation(
                "srv",
                current,
                telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                1,
                0
            )
            .await
            .is_err());
        assert!(
            Arc::ptr_eq(&previous, &registry.get_client_by_key("srv").await.unwrap()),
            "a partially prepared client must never replace the validated predecessor"
        );
        assert_installed(&registry).await;
        registry.disconnect("srv").await.unwrap();
    }
    #[tokio::test]
    async fn cached_schema_less_tools_do_not_depend_on_sibling_validator_availability() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        for worker in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
            let config = http_cfg("srv", "https://mcp.example.com/v1");
            let schema_tool = tool();
            let mut plain = schema_tool.clone();
            plain.tool_name = "plain".into();
            plain.full_name = "mcp__srv__plain".into();
            plain.output_schema = None;
            seed_entry_with_catalog(
                &store,
                &crate::discovery_cache::logical_cache_key(&config),
                0,
                ServerCapabilitiesDto {
                    tools: true,
                    ..Default::default()
                },
                vec![plain, schema_tool],
                vec![],
                vec![],
            );
            let mock = Arc::new(BridgeMock::new(&["worker"]));
            let validator = Arc::new(Validator::default());
            validator.fail.store(true, Ordering::SeqCst);
            let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
                .with_discovery_cache_store(store);
            let registry = if worker {
                registry.with_json_schema_validator(validator.clone())
            } else {
                registry
            };
            registry.connect(config).await.unwrap();
            let handle = registry.output_schema_parser_handle("srv", "mcp__srv__plain");
            let body = source_json(&serde_json::json!({"arbitrary":"payload"}));
            let parsed = handle
                .snapshot(None)
                .await
                .unwrap()
                .parse_output(&body)
                .await
                .unwrap();
            assert_eq!(parsed, body);
            assert_eq!(validator.compiles.load(Ordering::SeqCst), 0);
            if worker {
                assert!(registry
                    .output_schema_parser_handle("srv", "mcp__srv__worker")
                    .snapshot(None)
                    .await
                    .is_err());
                handle
                    .snapshot(None)
                    .await
                    .unwrap()
                    .parse_output(&body)
                    .await
                    .unwrap();
                assert_eq!(validator.compiles.load(Ordering::SeqCst), 1);
            }
            assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
        }
    }
}
