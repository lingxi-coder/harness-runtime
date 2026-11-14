mod cache_identity_retry {
    use super::*;
    use lingxi_core::host::json_schema_validator::{
        CompiledJsonSchema, JsonSchemaValidation, JsonSchemaValidatorSession, SchemaCompileError,
        SchemaProfile, SchemaWorkerError,
    };

    async fn save_grant(
        storage: &Arc<dyn SecureStorage>,
        clock: &Arc<dyn Clock>,
        config: &McpServerConfig,
        grant: &str,
    ) {
        oauth::store_tokens(
            storage,
            clock,
            &oauth::server_key(&config.name, &config.spec),
            &oauth::StoredTokens {
                access_token: "fixture-access".into(),
                refresh_token: Some(grant.into()),
                expires_at_millis: oauth::OAuthExpiry::Undefined,
                client_id: None,
                client_secret: None,
                step_up_scope: None,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn successful_cached_grant_rotation_retries_once_without_inheriting_old_catalogs() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        for rotate in [false, true] {
            for fail_catalogs in [false, true] {
                for background in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
                    let config = http_cfg("srv", "https://mcp.example.invalid/v1");
                    let storage: Arc<dyn SecureStorage> = Arc::new(XaaMemStorage::default());
                    let clock: Arc<dyn Clock> = Arc::new(FixedClock(std::time::UNIX_EPOCH));
                    save_grant(&storage, &clock, &config, "fixture-grant-a").await;
                    let mock = Arc::new(BridgeMock::with_drivable_calls(&["live"]));
                    let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
                        .with_oauth(OAuthDeps {
                            http: GatedXaaHttp::new(),
                            clock: clock.clone(),
                            storage: storage.clone(),
                            on_authorization_url: Arc::new(|_| panic!("unexpired fixture grant")),
                            xaa_config: None,
                        })
                        .with_discovery_cache_store(store.clone());
                    let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
                        &config.spec,
                        None,
                        mcp_connection_timeout().as_millis() as u64,
                    );
                    let original = registry
                        .discovery_cache_partition_for(&config, mode)
                        .await
                        .unwrap();
                    let mut old_tool = mock.tools[0].clone();
                    old_tool.tool_name = "cached_only".into();
                    old_tool.full_name = "mcp__srv__cached_only".into();
                    old_tool.server_name = "srv".into();
                    let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
                        original.logical_key.clone(),
                        crate::discovery_cache::now_ms(),
                        ServerCapabilitiesDto {
                            tools: true,
                            ..Default::default()
                        },
                        vec![old_tool],
                        vec![],
                        vec![],
                        vec![],
                    );
                    store
                        .store_partitioned(&entry, &original.partition_key)
                        .unwrap();
                    let cached = registry.connect(config.clone()).await.unwrap();
                    assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
                    let background_slot = if background {
                        let LazyUpgradePreparation::Wait(slot, true) = registry
                            .prepare_lazy_upgrade_slot_locked(
                                "srv",
                                LazyUpgradeMode::Background,
                                None,
                                Some("legacy".into()),
                                mode,
                            )
                            .await
                            .unwrap()
                        else {
                            panic!("background cached owner")
                        };
                        Some(slot)
                    } else {
                        None
                    };
                    if rotate {
                        save_grant(&storage, &clock, &config, "fixture-grant-b").await;
                    }
                    mock.list_tools_fails.store(fail_catalogs, Ordering::SeqCst);
                    if let Some(slot) = background_slot {
                        registry.spawn_lazy_upgrade_owner("srv".into(), slot);
                    }
                    let (first, waiter) = tokio::join!(
                        registry.ensure_connected_client("srv"),
                        registry.ensure_connected_client("srv")
                    );
                    let client = first.unwrap();
                    assert!(
                        Arc::ptr_eq(&client, &waiter.unwrap()),
                        "concurrent callers retain the same retry owner"
                    );
                    assert!(Arc::ptr_eq(
                        &client,
                        &registry.get_client("srv").await.unwrap()
                    ));
                    let live_id = {
                        let states = registry.connections.read().await;
                        let Some(McpConnectionState::Connected {
                            connection_id,
                            tools,
                            ..
                        }) = states.get("srv")
                        else {
                            panic!("the same lazy call must settle Connected")
                        };
                        let expected: &[&str] = match (rotate, fail_catalogs) {
                            (true, true) => &[],
                            (false, true) => &["cached_only"],
                            (_, false) => &["live"],
                        };
                        assert_eq!(
                            tools
                                .iter()
                                .map(|tool| tool.tool_name.as_str())
                                .collect::<Vec<_>>(),
                            expected
                        );
                        *connection_id
                    };
                    assert_ne!(live_id, cached);
                    assert_eq!(
                        mock.connect_calls.load(Ordering::SeqCst),
                        if rotate { 2 } else { 1 }
                    );
                    assert!(registry.lazy_upgrade_slot("srv").await.is_none());
                    assert!(registry.has_callable_server("srv").await);
                    assert!(registry.ensure_connected_client("srv").await.is_ok());
                    if !fail_catalogs {
                        let (tx, mut rx) = mock.take_tool_call_peer(live_id).unwrap();
                        for _ in 0..2 {
                            let call = registry.call_tool_with_auth_retry(
                                "srv",
                                "mcp__srv__live",
                                source_json(&serde_json::json!({})),
                                None,
                                None,
                            );
                            let respond = async {
                                let request = loop {
                                    let bytes =
                                        tokio::time::timeout(Duration::from_secs(2), rx.recv())
                                            .await
                                            .unwrap()
                                            .unwrap();
                                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                                    if request.get("id").is_some() {
                                        break request;
                                    }
                                };
                                assert_eq!(request["method"], "tools/call");
                                assert_eq!(request["params"]["name"], "live");
                                tx.send(Bytes::from(format!("{}\n", serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"content":[]}})))).await.unwrap();
                            };
                            let (result, ()) = tokio::join!(call, respond);
                            result.unwrap();
                        }
                        assert_eq!(
                            mock.connect_calls.load(Ordering::SeqCst),
                            if rotate { 2 } else { 1 },
                            "physical calls must not be replayed or redialed"
                        );
                    }
                    if rotate {
                        assert!(!registry
                            .prompt_predecessors
                            .read()
                            .await
                            .contains_key(&cached));
                        assert_eq!(
                            store.load_partitioned(&original.logical_key, &original.partition_key),
                            crate::discovery_cache::EntryLookup::Absent
                        );
                        let current = registry
                            .discovery_cache_partition_for(&config, mode)
                            .await
                            .unwrap();
                        let crate::discovery_cache::EntryLookup::Found(saved) =
                            store.load_partitioned(&current.logical_key, &current.partition_key)
                        else {
                            panic!("fresh grant catalog must be persisted in its own partition")
                        };
                        assert!(saved
                            .tools
                            .iter()
                            .all(|tool| tool.tool_name != "cached_only"));
                        assert_eq!(saved.consecutive_refresh_failures, 0);
                    }
                    registry.disconnect("srv").await.unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn grant_rotation_during_retry_schema_preparation_cannot_publish_the_old_grant() {
        struct Gate {
            pause: AtomicBool,
            entered: Notify,
            release: Notify,
        }
        struct Validator(Arc<Gate>);
        struct Session(Arc<Gate>);
        struct Plan;
        impl JsonSchemaValidator for Validator {
            fn new_session(&self, _: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
                Arc::new(Session(self.0.clone()))
            }
        }
        #[async_trait::async_trait]
        impl JsonSchemaValidatorSession for Session {
            async fn compile(
                &self,
                _: &Utf16JsonProjection,
            ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
                if self.0.pause.swap(false, Ordering::SeqCst) {
                    self.0.entered.notify_one();
                    self.0.release.notified().await;
                }
                Ok(Arc::new(Plan))
            }
        }
        #[async_trait::async_trait]
        impl CompiledJsonSchema for Plan {
            async fn validate(
                &self,
                _: &Utf16JsonProjection,
            ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
                Ok(JsonSchemaValidation::Valid)
            }
        }
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let config = http_cfg("srv", "https://mcp.example.invalid/v1");
        let storage: Arc<dyn SecureStorage> = Arc::new(XaaMemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(std::time::UNIX_EPOCH));
        save_grant(&storage, &clock, &config, "fixture-grant-a").await;
        let mut mock = BridgeMock::new(&["live"]);
        mock.tools[0].output_schema = Some(source_json(&serde_json::json!({"required":["ok"]})));
        let mock = Arc::new(mock);
        let gate = Arc::new(Gate {
            pause: AtomicBool::new(true),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let registry = Arc::new(
            McpRegistry::with_raw_conn(mock.clone(), mock.clone())
                .with_json_schema_validator(Arc::new(Validator(gate.clone())))
                .with_oauth(OAuthDeps {
                    http: GatedXaaHttp::new(),
                    clock: clock.clone(),
                    storage: storage.clone(),
                    on_authorization_url: Arc::new(|_| panic!("unexpired fixture")),
                    xaa_config: None,
                })
                .with_discovery_cache_store(store.clone()),
        );
        let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            None,
            mcp_connection_timeout().as_millis() as u64,
        );
        let old = registry
            .discovery_cache_partition_for(&config, mode)
            .await
            .unwrap();
        let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
            old.logical_key.clone(),
            crate::discovery_cache::now_ms(),
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store.store_partitioned(&entry, &old.partition_key).unwrap();
        registry.connect(config.clone()).await.unwrap();
        save_grant(&storage, &clock, &config, "fixture-grant-b").await;
        let rejected = registry
            .discovery_cache_partition_for(&config, mode)
            .await
            .unwrap();
        let pending = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        save_grant(&storage, &clock, &config, "fixture-grant-c").await;
        gate.release.notify_one();
        let error = match pending.await.unwrap() {
            Ok(_) => panic!("rotated grant published after preparation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("grant changed while preparing"));
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected {
                last_error: Some(_),
                ..
            })
        ));
        assert!(registry.lazy_upgrade_slot("srv").await.is_none());
        assert!(registry.get_client("srv").await.is_none());
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            2,
            "retry remains bounded"
        );
        assert_eq!(
            store.load_partitioned(&rejected.logical_key, &rejected.partition_key),
            crate::discovery_cache::EntryLookup::Absent
        );
        registry.connect(config).await.unwrap();
        registry.ensure_connected_client("srv").await.unwrap();
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 3);
        registry.disconnect("srv").await.unwrap();
    }

    #[tokio::test]
    async fn a_second_grant_rotation_finishes_retryable_without_retiring_the_new_partition() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let config = http_cfg("srv", "https://mcp.example.invalid/v1");
        let storage: Arc<dyn SecureStorage> = Arc::new(XaaMemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(FixedClock(std::time::UNIX_EPOCH));
        save_grant(&storage, &clock, &config, "fixture-grant-a").await;
        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
            .with_oauth(OAuthDeps {
                http: GatedXaaHttp::new(),
                clock: clock.clone(),
                storage: storage.clone(),
                on_authorization_url: Arc::new(|_| panic!("unexpired fixture")),
                xaa_config: None,
            })
            .with_discovery_cache_store(store.clone());
        let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            None,
            mcp_connection_timeout().as_millis() as u64,
        );
        let old = registry
            .discovery_cache_partition_for(&config, mode)
            .await
            .unwrap();
        let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
            old.logical_key.clone(),
            crate::discovery_cache::now_ms(),
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store.store_partitioned(&entry, &old.partition_key).unwrap();
        registry.connect(config.clone()).await.unwrap();
        let LazyUpgradePreparation::Wait(slot, true) = registry
            .prepare_lazy_upgrade_slot_locked("srv", LazyUpgradeMode::Foreground, None, None, mode)
            .await
            .unwrap()
        else {
            panic!("cached owner")
        };
        save_grant(&storage, &clock, &config, "fixture-grant-b").await;
        let first = registry
            .discover_live_connection(&config, mode)
            .await
            .unwrap();
        let BackgroundInstallOutcome::Rejected(first) = registry
            .install_lazy_upgrade_live_discovery_if_current("srv", &slot, first)
            .await
        else {
            panic!("old cache partition must be rejected")
        };
        assert!(
            registry
                .begin_fresh_lazy_upgrade_retry_locked("srv", &slot)
                .await
        );
        registry.discard_live_discovery(first).await;
        let retry = registry
            .discover_live_connection(&config, mode)
            .await
            .unwrap();
        save_grant(&storage, &clock, &config, "fixture-grant-c").await;
        let current = registry
            .discovery_cache_partition_for(&config, mode)
            .await
            .unwrap();
        store
            .store_partitioned(&entry, &current.partition_key)
            .unwrap();
        let BackgroundInstallOutcome::Rejected(retry) = registry
            .install_lazy_upgrade_live_discovery_if_current("srv", &slot, retry)
            .await
        else {
            panic!("the retry's grant is superseded too")
        };
        assert!(
            !registry
                .begin_fresh_lazy_upgrade_retry_locked("srv", &slot)
                .await,
            "no third discovery"
        );
        registry
            .finish_lazy_upgrade_rejection_locked("srv", &slot)
            .await;
        registry.discard_live_discovery(retry).await;
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected {
                last_error: Some(_),
                ..
            })
        ));
        assert!(registry.lazy_upgrade_slot("srv").await.is_none());
        assert!(registry.get_client("srv").await.is_none());
        assert!(
            matches!(store.load_partitioned(&current.logical_key, &current.partition_key), crate::discovery_cache::EntryLookup::Found(saved) if saved.consecutive_refresh_failures == 0)
        );
        registry.connect(config).await.unwrap();
        registry.ensure_connected_client("srv").await.unwrap();
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            3,
            "the current grant remains recoverable"
        );
        registry.disconnect("srv").await.unwrap();
    }

    #[tokio::test]
    async fn fresh_retry_cleanup_preserves_superseding_live_generation() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let config = http_cfg("srv", "https://mcp.example.invalid/v1");
        seed_entry(
            &store,
            &crate::discovery_cache::logical_cache_key(&config),
            0,
        );
        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry = McpRegistry::with_raw_conn(mock.clone(), mock.clone())
            .with_discovery_cache_store(store);
        registry.connect(config.clone()).await.unwrap();
        let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            None,
            mcp_connection_timeout().as_millis() as u64,
        );
        let LazyUpgradePreparation::Wait(slot, true) = registry
            .prepare_lazy_upgrade_slot_locked("srv", LazyUpgradeMode::Foreground, None, None, mode)
            .await
            .unwrap()
        else {
            panic!("fresh cached owner")
        };
        assert!(
            registry
                .begin_fresh_lazy_upgrade_retry_locked("srv", &slot)
                .await
        );
        assert!(
            !registry
                .begin_fresh_lazy_upgrade_retry_locked("srv", &slot)
                .await,
            "retry is bounded once"
        );
        let mut replacement = config;
        replacement.timeout_ms = Some(42_000);
        let replacement_id = registry
            .replace_config_atomically(replacement)
            .await
            .unwrap()
            .unwrap();
        let replacement_client = registry.get_client("srv").await.unwrap();
        registry
            .finish_lazy_upgrade_rejection_locked("srv", &slot)
            .await;
        assert!(
            matches!(registry.connections.read().await.get("srv"), Some(McpConnectionState::Connected { connection_id, .. }) if *connection_id == replacement_id)
        );
        assert!(Arc::ptr_eq(
            &replacement_client,
            &registry.get_client("srv").await.unwrap()
        ));
        assert!(registry.lazy_upgrade_slot("srv").await.is_none());
        registry.disconnect("srv").await.unwrap();
    }

    #[tokio::test]
    async fn fresh_retry_worker_failure_cannot_restore_the_retired_cached_authority() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let config = http_cfg("srv", "https://mcp.example.invalid/v1");
        seed_entry(
            &store,
            &crate::discovery_cache::logical_cache_key(&config),
            0,
        );
        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry =
            McpRegistry::with_raw_conn(mock.clone(), mock).with_discovery_cache_store(store);
        registry.connect(config.clone()).await.unwrap();
        let retained = registry.output_schema_parser_handle("srv", "mcp__srv__alpha");
        retained.snapshot(None).await.unwrap();
        let mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            None,
            mcp_connection_timeout().as_millis() as u64,
        );
        let LazyUpgradePreparation::Wait(slot, true) = registry
            .prepare_lazy_upgrade_slot_locked("srv", LazyUpgradeMode::Foreground, None, None, mode)
            .await
            .unwrap()
        else {
            panic!("fresh cached owner")
        };
        assert!(
            registry
                .begin_fresh_lazy_upgrade_retry_locked("srv", &slot)
                .await
        );
        assert!(
            retained
                .snapshot(Some(&source_json(&serde_json::json!({"type":"object"}))))
                .await
                .is_err(),
            "retirement must not recompile the selected old DTO as a new authority"
        );
        registry
            .finish_lazy_upgrade_preparation_failure_locked(
                "srv",
                &slot,
                &McpError::Connection("local schema worker failed".into()),
            )
            .await;
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected {
                last_error: Some(_),
                ..
            })
        ));
        assert!(registry.lazy_upgrade_slot("srv").await.is_none());
        let parser = registry.output_parser_slot("srv");
        let binding = parser.read().await;
        assert!(binding.client.is_none() && binding.cached.is_none());
    }
}
