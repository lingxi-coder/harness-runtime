mod auth_recovery_and_waiters {
    use super::*;
    use lingxi_core::host::http::{HttpError, SseStream};
    use lingxi_core::host::secure_storage::InMemorySecureStorage;
    use lingxi_core::host::McpOAuthConfigDto;
    use lingxi_core::types::{HttpRequest, HttpResponse};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct Peer {
        dials: AtomicUsize,
        calls: Arc<AtomicUsize>,
        reject: Arc<AtomicUsize>,
        call_entered: Arc<tokio::sync::Notify>,
        call_release: Arc<tokio::sync::Notify>,
        list_gate: AtomicBool,
        list_entered: tokio::sync::Notify,
        list_release: tokio::sync::Notify,
        disconnected: tokio::sync::Notify,
        conns: StdMutex<HashMap<McpConnectionId, Arc<jsonrpc::Connection>>>,
        bearers: StdMutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl McpTransport for Peer {
        async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            let McpTransportSpec::Http { headers, .. } = spec else {
                panic!("HTTP fixture")
            };
            self.bearers.lock().unwrap().push(
                headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default(),
            );
            self.dials.fetch_add(1, Ordering::SeqCst);
            let id = McpConnectionId::new();
            let (to_client, incoming) = mpsc::channel::<Bytes>(8);
            let (outgoing, mut from_client) = mpsc::channel::<Bytes>(8);
            self.conns.lock().unwrap().insert(
                id,
                Arc::new(jsonrpc::Connection::new_streams(
                    incoming,
                    outgoing,
                    jsonrpc::Mode::Lines,
                )),
            );
            let calls = self.calls.clone();
            let reject = self.reject.clone();
            let entered = self.call_entered.clone();
            let release = self.call_release.clone();
            tokio::spawn(async move {
                while let Some(frame) = from_client.recv().await {
                    let request: Value = serde_json::from_slice(&frame).unwrap();
                    if request.get("id").is_none() {
                        continue;
                    }
                    assert_eq!(request["method"], "tools/call");
                    calls.fetch_add(1, Ordering::SeqCst);
                    let status = reject.swap(0, Ordering::SeqCst);
                    let response = if status != 0 {
                        entered.notify_one();
                        release.notified().await;
                        serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32001,"message":"rejected old bearer","data":{"httpStatus":status}}})
                    } else {
                        serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"content":[],"structuredContent":{"ok":true}}})
                    };
                    if to_client
                        .send(Bytes::from(format!("{response}\n")))
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
            if self.list_gate.swap(false, Ordering::SeqCst) {
                self.list_entered.notify_one();
                self.list_release.notified().await;
            }
            Ok(vec![McpToolDto {
                full_name: "mcp__srv__ping".into(),
                server_name: "srv".into(),
                tool_name: "ping".into(),
                wire_tool_name: None,
                description: "fixture".into(),
                input_schema: source_json(&serde_json::json!({})),
                output_schema: None,
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
            _: Utf16JsonProjection,
        ) -> Result<McpToolResultDto, McpError> {
            panic!("use physical JSON-RPC")
        }
        async fn read_resource(
            &self,
            _: &McpRawConnection,
            _: &Utf16JsonProjection,
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
            self.conns.lock().unwrap().remove(&id);
            self.disconnected.notify_one();
            Ok(())
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Http]
        }
    }
    impl RawConnectionProvider for Peer {
        fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
            self.conns.lock().unwrap().get(&id).cloned()
        }
    }

    #[derive(Default)]
    struct Http {
        requests: StdMutex<Vec<String>>,
        gate: AtomicBool,
        fail: AtomicBool,
        invalid_grant: AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl HttpTransport for Http {
        async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(request.url.clone());
            let body = if request.url.ends_with("/token") {
                if self.gate.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                if self.fail.load(Ordering::SeqCst) {
                    return Err(HttpError::Connection("refresh fixture unavailable".into()));
                }
                if self.invalid_grant.load(Ordering::SeqCst) {
                    return Ok(HttpResponse {
                        status: 400,
                        headers: vec![],
                        body: r#"{"error":"invalid_grant"}"#.into(),
                        body_bytes: vec![],
                    });
                }
                serde_json::json!({"access_token":"access-b","refresh_token":"refresh-b","token_type":"Bearer","expires_in":3600})
            } else if request.url.ends_with("/revoke") {
                serde_json::json!({})
            } else {
                assert!(
                    request.url.contains(".well-known"),
                    "no automatic revocation: {}",
                    request.url
                );
                serde_json::json!({"issuer":"https://as.example.test","authorization_endpoint":"https://as.example.test/authorize","token_endpoint":"https://as.example.test/token","revocation_endpoint":"https://as.example.test/revoke","response_types_supported":["code"],"grant_types_supported":["refresh_token","authorization_code"],"code_challenge_methods_supported":["S256"]})
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: body.to_string(),
                body_bytes: vec![],
            })
        }
        async fn stream_sse(&self, _: HttpRequest) -> Result<SseStream, HttpError> {
            panic!("unused")
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        config: McpServerConfig,
        storage: Arc<dyn SecureStorage>,
        clock: Arc<dyn Clock>,
        peer: Arc<Peer>,
        http: Arc<Http>,
        registry: Arc<McpRegistry>,
        key: String,
    }
    impl Fixture {
        async fn new(cached: bool) -> Self {
            Self::new_with_publish_gate(cached, None).await
        }
        async fn new_with_publish_gate(cached: bool, publish: Option<Arc<TestPauseHook>>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let mut config = http_cfg("srv", "https://mcp.example.test/v1");
            config.discovery_cache = Some(true);
            let McpTransportSpec::Http { oauth, .. } = &mut config.spec else {
                unreachable!()
            };
            *oauth = Some(McpOAuthConfigDto {
                client_id: Some("fixture-client".into()),
                callback_port: None,
                auth_server_metadata_url: Some(
                    "https://as.example.test/.well-known/oauth-authorization-server".into(),
                ),
                scopes: None,
                xaa: None,
            });
            let storage: Arc<dyn SecureStorage> = Arc::new(InMemorySecureStorage::new());
            let clock: Arc<dyn Clock> = Arc::new(FixedClock(
                std::time::UNIX_EPOCH + Duration::from_secs(1000),
            ));
            let peer = Arc::new(Peer::default());
            let http = Arc::new(Http::default());
            let make = || {
                let mut registry = McpRegistry::with_raw_conn(peer.clone(), peer.clone())
                    .with_oauth(OAuthDeps {
                        http: http.clone(),
                        clock: clock.clone(),
                        storage: storage.clone(),
                        on_authorization_url: Arc::new(|_| {
                            panic!("valid refresh never opens a browser")
                        }),
                        xaa_config: None,
                    })
                    .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(
                        directory.path(),
                    ))
                    .with_mcp_oauth_identity(oauth::McpOAuthIdentity {
                        refresh_lock_dir: Some(directory.path().join("refresh-locks")),
                        ..Default::default()
                    });
                if let Some(hook) = &publish {
                    registry = registry.with_pause_before_client_publish(hook.clone());
                }
                Arc::new(registry)
            };
            let key = oauth::server_key(config.storage_name(), &config.spec);
            save(&storage, &clock, &key, "a").await;
            let registry = make();
            if let Some(hook) = &publish {
                hook.release.notify_one();
            }
            registry.connect(config.clone()).await.unwrap();
            if let Some(hook) = &publish {
                entered(&hook.entered).await;
            }
            let registry = if cached {
                let reader = make();
                reader.connect(config.clone()).await.unwrap();
                assert!(matches!(
                    reader.connections.read().await.get("srv"),
                    Some(McpConnectionState::Cached { .. })
                ));
                assert_eq!(
                    peer.dials.load(Ordering::SeqCst),
                    1,
                    "Cached admission never dials"
                );
                reader
            } else {
                registry
            };
            Self {
                _directory: directory,
                config,
                storage,
                clock,
                peer,
                http,
                registry,
                key,
            }
        }
        fn spawn_call(
            &self,
        ) -> tokio::task::JoinHandle<Result<McpToolResultDto, crate::client::McpClientError>>
        {
            let registry = self.registry.clone();
            tokio::spawn(async move {
                registry
                    .call_tool_with_auth_retry(
                        "srv",
                        "mcp__srv__ping",
                        source_json(&serde_json::json!({})),
                        None,
                        None,
                    )
                    .await
            })
        }
        async fn reject_first(
            &self,
            status: usize,
        ) -> tokio::task::JoinHandle<Result<McpToolResultDto, crate::client::McpClientError>>
        {
            self.peer.reject.store(status, Ordering::SeqCst);
            let call = self.spawn_call();
            entered(&self.peer.call_entered).await;
            call
        }
        async fn stored(&self) -> oauth::StoredTokens {
            oauth::load_tokens(&self.storage, &self.key)
                .await
                .unwrap()
                .unwrap()
        }
    }
    async fn save(
        storage: &Arc<dyn SecureStorage>,
        clock: &Arc<dyn Clock>,
        key: &str,
        suffix: &str,
    ) {
        oauth::store_tokens(
            storage,
            clock,
            key,
            &oauth::StoredTokens {
                access_token: format!("access-{suffix}"),
                refresh_token: Some(format!("refresh-{suffix}")),
                expires_at_millis: oauth::OAuthExpiry::Undefined,
                client_id: Some("fixture-client".into()),
                client_secret: None,
                step_up_scope: None,
            },
        )
        .await
        .unwrap();
    }
    async fn entered(notify: &tokio::sync::Notify) {
        tokio::time::timeout(Duration::from_secs(5), notify.notified())
            .await
            .expect("fixture reached its publication barrier");
    }
    async fn settled<T>(task: tokio::task::JoinHandle<T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("caller settled")
            .expect("caller did not panic")
    }
    fn guard(
        identity: crate::tool_identity::McpToolIdentity,
        allowed: Arc<AtomicBool>,
    ) -> crate::tool_identity::McpToolDispatchGuard {
        Arc::new(move |current| current == &identity && allowed.load(Ordering::SeqCst))
    }

    fn ordinary_registry(fixture: &Fixture) -> Arc<McpRegistry> {
        Arc::new(
            McpRegistry::with_raw_conn(fixture.peer.clone(), fixture.peer.clone())
                .with_mcp_oauth_identity(fixture.registry.oauth_identity.clone())
                .with_oauth(OAuthDeps {
                    http: fixture.http.clone(),
                    clock: fixture.clock.clone(),
                    storage: fixture.storage.clone(),
                    on_authorization_url: Arc::new(|_| panic!("no browser")),
                    xaa_config: None,
                }),
        )
    }

    async fn expire_a(fixture: &Fixture) {
        let mut token = fixture.stored().await;
        token.expires_at_millis =
            oauth::OAuthExpiry::from_system_time(fixture.clock.now() - Duration::from_secs(1));
        oauth::store_tokens(&fixture.storage, &fixture.clock, &fixture.key, &token)
            .await
            .unwrap();
    }

    struct RefreshStorageFault {
        base: Arc<dyn SecureStorage>,
        capture: AtomicBool,
        commit: AtomicBool,
        after_publication: bool,
    }
    #[async_trait::async_trait]
    impl SecureStorage for RefreshStorageFault {
        async fn store(
            &self,
            service: &str,
            key: &str,
            data: lingxi_core::types::SecureStorageData,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            let fail = self.commit.swap(false, Ordering::SeqCst);
            if !fail || self.after_publication {
                self.base.store(service, key, data).await?;
            }
            if fail {
                return Err(lingxi_core::host::SecureStorageError::Io(
                    "synthetic commit failure".into(),
                ));
            }
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            key: &str,
        ) -> Result<
            Option<lingxi_core::types::SecureStorageData>,
            lingxi_core::host::SecureStorageError,
        > {
            self.base.retrieve(service, key).await
        }
        async fn retrieve_uncached(
            &self,
            service: &str,
            key: &str,
        ) -> Result<
            Option<lingxi_core::types::SecureStorageData>,
            lingxi_core::host::SecureStorageError,
        > {
            if self.capture.swap(false, Ordering::SeqCst) {
                return Err(lingxi_core::host::SecureStorageError::BackendUnavailable(
                    "synthetic capture failure".into(),
                ));
            }
            self.base.retrieve_uncached(service, key).await
        }
        async fn delete(
            &self,
            service: &str,
            key: &str,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            self.base.delete(service, key).await
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
            self.base.list(service).await
        }
        fn is_encrypted(&self) -> bool {
            self.base.is_encrypted()
        }
        fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
            self.base.backend()
        }
    }

    #[tokio::test]
    async fn ordinary_refresh_storage_errors_preserve_grant_and_do_not_request_login() {
        for (capture, after_publication) in [(true, false), (false, false), (false, true)] {
            let fixture = Fixture::new(false).await;
            expire_a(&fixture).await;
            let original = fixture.stored().await;
            let storage = Arc::new(RefreshStorageFault {
                base: fixture.storage.clone(),
                capture: AtomicBool::new(capture),
                commit: AtomicBool::new(!capture),
                after_publication,
            });
            let registry = McpRegistry::with_raw_conn(fixture.peer.clone(), fixture.peer.clone())
                .with_mcp_oauth_identity(fixture.registry.oauth_identity.clone())
                .with_oauth(OAuthDeps {
                    http: fixture.http.clone(),
                    clock: fixture.clock.clone(),
                    storage,
                    on_authorization_url: Arc::new(|_| {
                        panic!("storage failure must not open login")
                    }),
                    xaa_config: None,
                });
            let error = registry.connect(fixture.config.clone()).await.unwrap_err();
            assert!(matches!(error, McpError::Connection(_)), "{error}");
            assert!(matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Disconnected {
                    last_error: Some(_),
                    ..
                })
            ));
            assert_eq!(
                fixture.stored().await,
                original,
                "failed commit compensates token publication"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_refresh_preserves_new_token_and_session_on_success_and_failure() {
        for (fail, invalid) in [(false, false), (true, false), (false, true)] {
            let fixture = Fixture::new(false).await;
            expire_a(&fixture).await;
            fixture.http.gate.store(true, Ordering::SeqCst);
            fixture.http.fail.store(fail, Ordering::SeqCst);
            fixture.http.invalid_grant.store(invalid, Ordering::SeqCst);
            let owner = ordinary_registry(&fixture);
            let task = {
                let owner = owner.clone();
                let config = fixture.config.clone();
                tokio::spawn(async move { owner.connect(config).await })
            };
            entered(&fixture.http.entered).await;
            let mut session = oauth::session::load(&fixture.storage, &fixture.key)
                .await
                .unwrap();
            session.client_registration = Some("new-owner-registration".into());
            oauth::session::save(&fixture.storage, &fixture.clock, &fixture.key, &session)
                .await
                .unwrap();
            save(&fixture.storage, &fixture.clock, &fixture.key, "c").await;
            fixture.http.release.notify_one();
            settled(task).await.unwrap();
            assert_eq!(fixture.stored().await.access_token, "access-c");
            assert_eq!(
                oauth::session::load(&fixture.storage, &fixture.key)
                    .await
                    .unwrap()
                    .client_registration
                    .as_deref(),
                Some("new-owner-registration")
            );
            assert_eq!(
                *fixture.peer.bearers.lock().unwrap(),
                ["Bearer access-a", "Bearer access-c"]
            );
            owner
                .call_tool_with_auth_retry(
                    "srv",
                    "mcp__srv__ping",
                    source_json(&serde_json::json!({})),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn ordinary_refresh_does_not_resurrect_a_concurrent_logout() {
        let fixture = Fixture::new(false).await;
        expire_a(&fixture).await;
        fixture.http.gate.store(true, Ordering::SeqCst);
        let owner = ordinary_registry(&fixture);
        let task = {
            let config = fixture.config.clone();
            tokio::spawn(async move { owner.connect(config).await })
        };
        entered(&fixture.http.entered).await;
        fixture.registry.disconnect("srv").await.unwrap();
        fixture.http.release.notify_one();
        assert!(settled(task).await.is_err());
        assert!(oauth::load_tokens(&fixture.storage, &fixture.key)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn canceled_auth_http_or_catalog_wait_can_continue_without_manual_reconnect() {
        for catalog_wait in [false, true] {
            for status in [401, 403] {
                let fixture = Fixture::new(false).await;
                if catalog_wait {
                    fixture.peer.list_gate.store(true, Ordering::SeqCst);
                    save(&fixture.storage, &fixture.clock, &fixture.key, "c").await;
                } else {
                    fixture.http.gate.store(true, Ordering::SeqCst);
                }
                let call = fixture.reject_first(status).await;
                fixture.peer.call_release.notify_one();
                if catalog_wait {
                    entered(&fixture.peer.list_entered).await;
                } else {
                    entered(&fixture.http.entered).await;
                }
                call.abort();
                assert!(call.await.unwrap_err().is_cancelled());
                if catalog_wait {
                    entered(&fixture.peer.disconnected).await;
                }
                assert!(matches!(
                    fixture.registry.connections.read().await.get("srv"),
                    Some(McpConnectionState::Connected { .. })
                ));
                assert_eq!(
                    fixture.peer.conns.lock().unwrap().len(),
                    1,
                    "only the registered generation remains"
                );
                save(&fixture.storage, &fixture.clock, &fixture.key, "c").await;
                // A real server keeps rejecting the old bearer, so retry must
                // adopt C rather than merely succeeding once with old A.
                let resumed = fixture.reject_first(status).await;
                fixture.peer.call_release.notify_one();
                settled(resumed).await.unwrap();
                assert_eq!(fixture.stored().await.access_token, "access-c");
                assert_eq!(
                    fixture.peer.bearers.lock().unwrap().last().unwrap(),
                    "Bearer access-c"
                );
                assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 3);
                assert_eq!(fixture.peer.conns.lock().unwrap().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn canceled_auth_publication_settles_without_replaying_rpc_and_respects_logout() {
        for logout in [false, true] {
            let publish = Arc::new(TestPauseHook::default());
            let fixture = Fixture::new_with_publish_gate(false, Some(publish.clone())).await;
            let call = fixture.reject_first(401).await;
            save(&fixture.storage, &fixture.clock, &fixture.key, "c").await;
            fixture.peer.call_release.notify_one();
            entered(&publish.entered).await;
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
            if logout {
                let registry = fixture.registry.clone();
                let stopped = tokio::spawn(async move { registry.disconnect("srv").await });
                publish.release.notify_one();
                settled(stopped).await.unwrap();
                assert!(oauth::load_tokens(&fixture.storage, &fixture.key)
                    .await
                    .unwrap()
                    .is_none());
                assert!(!fixture.registry.has_callable_server("srv").await);
                assert_eq!(
                    fixture.peer.calls.load(Ordering::SeqCst),
                    1,
                    "publication must not replay the canceled RPC"
                );
            } else {
                let continued = fixture.spawn_call();
                publish.release.notify_one();
                settled(continued).await.unwrap();
                assert_eq!(fixture.stored().await.access_token, "access-c");
                assert_eq!(
                    fixture.peer.calls.load(Ordering::SeqCst),
                    2,
                    "one canceled call plus one new call"
                );
                assert_eq!(
                    fixture.peer.bearers.lock().unwrap().last().unwrap(),
                    "Bearer access-c"
                );
            }
        }
    }

    #[tokio::test]
    async fn stale_401_and_403_preserve_new_grant_and_retry_its_actual_bearer() {
        for status in [401, 403] {
            let fixture = Fixture::new(false).await;
            let call = fixture.reject_first(status).await;
            save(&fixture.storage, &fixture.clock, &fixture.key, "c").await;
            fixture.peer.call_release.notify_one();
            settled(call).await.unwrap();
            assert_eq!(fixture.stored().await.access_token, "access-c");
            assert_eq!(
                *fixture.peer.bearers.lock().unwrap(),
                ["Bearer access-a", "Bearer access-c"]
            );
            assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 2);
            assert!(
                fixture.http.requests.lock().unwrap().is_empty(),
                "a valid newer credential does not need refresh or revocation"
            );
        }
    }

    #[tokio::test]
    async fn same_rejected_grant_refreshes_once_without_logging_the_user_out() {
        let fixture = Fixture::new(false).await;
        let call = fixture.reject_first(401).await;
        fixture.peer.call_release.notify_one();
        settled(call).await.unwrap();
        assert_eq!(fixture.stored().await.access_token, "access-b");
        assert_eq!(
            *fixture.peer.bearers.lock().unwrap(),
            ["Bearer access-a", "Bearer access-b"]
        );
        assert_eq!(
            fixture
                .http
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|url| url.ends_with("/token"))
                .count(),
            1
        );
        assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn newer_grant_and_session_win_during_successful_or_failed_refresh_network_wait() {
        for (fail, invalid_grant) in [(false, false), (true, false), (false, true)] {
            let fixture = Fixture::new(false).await;
            fixture.http.gate.store(true, Ordering::SeqCst);
            fixture.http.fail.store(fail, Ordering::SeqCst);
            fixture
                .http
                .invalid_grant
                .store(invalid_grant, Ordering::SeqCst);
            let call = fixture.reject_first(401).await;
            fixture.peer.call_release.notify_one();
            entered(&fixture.http.entered).await;
            let mut session = oauth::session::load(&fixture.storage, &fixture.key)
                .await
                .unwrap();
            session.client_registration = Some("new-owner-registration".into());
            oauth::session::save(&fixture.storage, &fixture.clock, &fixture.key, &session)
                .await
                .unwrap();
            save(&fixture.storage, &fixture.clock, &fixture.key, "c").await;
            fixture.http.release.notify_one();
            settled(call).await.unwrap();
            assert_eq!(fixture.stored().await.access_token, "access-c");
            assert_eq!(
                oauth::session::load(&fixture.storage, &fixture.key)
                    .await
                    .unwrap()
                    .client_registration
                    .as_deref(),
                Some("new-owner-registration")
            );
            assert_eq!(
                *fixture.peer.bearers.lock().unwrap(),
                ["Bearer access-a", "Bearer access-c"]
            );
            assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn explicit_disconnect_still_revokes_and_clears_current_grant() {
        let fixture = Fixture::new(false).await;
        fixture.registry.disconnect("srv").await.unwrap();
        assert!(oauth::load_tokens(&fixture.storage, &fixture.key)
            .await
            .unwrap()
            .is_none());
        assert!(!fixture.registry.has_callable_server("srv").await);
    }

    #[tokio::test]
    async fn explicit_logout_during_an_old_call_is_not_undone_by_its_late_401() {
        let fixture = Fixture::new(false).await;
        let call = fixture.reject_first(401).await;
        fixture.registry.disconnect("srv").await.unwrap();
        fixture.peer.call_release.notify_one();
        assert!(settled(call).await.is_err());
        assert!(oauth::load_tokens(&fixture.storage, &fixture.key)
            .await
            .unwrap()
            .is_none());
        assert!(!fixture.registry.has_callable_server("srv").await);
        assert_eq!(fixture.peer.dials.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture
                .http
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|url| url.ends_with("/token"))
                .count(),
            0,
            "a retired connection cannot restart authentication"
        );
    }

    #[tokio::test]
    async fn leased_late_waiters_join_one_cached_owner_instead_of_reporting_revocation() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        for cancel_first_waiter in [false, true] {
            let fixture = Fixture::new(true).await;
            fixture.peer.list_gate.store(true, Ordering::SeqCst);
            let owner = {
                let registry = fixture.registry.clone();
                tokio::spawn(async move { registry.ensure_connected_client("srv").await })
            };
            entered(&fixture.peer.list_entered).await;
            if cancel_first_waiter {
                owner.abort();
            }
            let identity = fixture
                .registry
                .lazy_upgrade_slot("srv")
                .await
                .unwrap()
                .cached_state
                .as_ref()
                .and_then(|state| match state {
                    McpConnectionState::Cached { config, tools, .. } => Some(
                        crate::tool_identity::McpToolIdentity::from_config_tool(config, &tools[0]),
                    ),
                    _ => None,
                })
                .unwrap();
            let allowed = Arc::new(AtomicBool::new(true));
            let mut waiter = Box::pin(fixture.registry.call_tool_with_auth_retry_guarded(
                "srv",
                "mcp__srv__ping",
                source_json(&serde_json::json!({})),
                None,
                None,
                Some(guard(identity, allowed)),
            ));
            assert!(
                futures_util::poll!(waiter.as_mut()).is_pending(),
                "late leased caller must wait for its owned Connecting slot"
            );
            fixture.peer.list_release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .unwrap()
                .unwrap();
            if !cancel_first_waiter {
                settled(owner).await.unwrap();
            }
            assert_eq!(
                fixture.peer.dials.load(Ordering::SeqCst),
                2,
                "one warm connection and one single-flight lazy dial"
            );
            assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn leased_waiter_rechecks_revocation_after_its_cached_owner_settles() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let fixture = Fixture::new(true).await;
        let identity = fixture
            .registry
            .tool_identity_catalog()
            .await
            .into_iter()
            .next()
            .unwrap();
        let allowed = Arc::new(AtomicBool::new(true));
        fixture.peer.list_gate.store(true, Ordering::SeqCst);
        let owner = {
            let registry = fixture.registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        entered(&fixture.peer.list_entered).await;
        let mut waiter = Box::pin(fixture.registry.call_tool_with_auth_retry_guarded(
            "srv",
            "mcp__srv__ping",
            source_json(&serde_json::json!({})),
            None,
            None,
            Some(guard(identity, allowed.clone())),
        ));
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        allowed.store(false, Ordering::SeqCst);
        fixture.peer.list_release.notify_one();
        settled(owner).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("revoked"));
        assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn replacement_config_cannot_be_called_by_a_waiter_holding_the_old_capability() {
        let _lock = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");
        let fixture = Fixture::new(true).await;
        let identity = fixture
            .registry
            .tool_identity_catalog()
            .await
            .into_iter()
            .next()
            .unwrap();
        fixture.peer.list_gate.store(true, Ordering::SeqCst);
        let owner = {
            let registry = fixture.registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        entered(&fixture.peer.list_entered).await;
        let mut waiter = Box::pin(fixture.registry.call_tool_with_auth_retry_guarded(
            "srv",
            "mcp__srv__ping",
            source_json(&serde_json::json!({})),
            None,
            None,
            Some(guard(identity, Arc::new(AtomicBool::new(true)))),
        ));
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        let mut replacement = fixture.config.clone();
        replacement.always_load = !replacement.always_load;
        let replacement_id = fixture
            .registry
            .replace_config_atomically(replacement)
            .await
            .unwrap()
            .unwrap();
        fixture.peer.list_release.notify_one();
        assert!(settled(owner).await.is_err());
        assert!(tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .is_err());
        assert!(
            matches!(fixture.registry.connections.read().await.get("srv"), Some(McpConnectionState::Connected {connection_id, ..}) if *connection_id == replacement_id)
        );
        assert_eq!(fixture.peer.calls.load(Ordering::SeqCst), 0);
        let current = fixture
            .registry
            .tool_identity_catalog()
            .await
            .into_iter()
            .next()
            .unwrap();
        fixture
            .registry
            .call_tool_with_auth_retry_guarded(
                "srv",
                "mcp__srv__ping",
                source_json(&serde_json::json!({})),
                None,
                None,
                Some(guard(current, Arc::new(AtomicBool::new(true)))),
            )
            .await
            .unwrap();
        assert_eq!(
            fixture.peer.calls.load(Ordering::SeqCst),
            1,
            "the fresh explicitly granted capability stays callable"
        );
    }
}
