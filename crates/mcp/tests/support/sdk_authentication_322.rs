//! SDK auth uses the existing fake AS plus real loopback callback sockets.
use super::*;
use mcp::McpConnectionState;
use serde_json::Value;

struct SdkAs {
    inner: Arc<MockAs>,
    reject_custom: bool,
    reject_all: bool,
}
#[async_trait]
impl HttpTransport for SdkAs {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        if request.url.ends_with("/register")
            && (self.reject_all
                || self.reject_custom
                    && !request
                        .body
                        .as_deref()
                        .unwrap_or_default()
                        .contains("http://localhost:"))
        {
            self.inner.requests.lock().unwrap().push(request);
            return Ok(HttpResponse {status:400,headers:vec![],body:r#"{"error":"invalid_redirect_uri","error_description":"LOCAL_REDIRECT_REJECTED"}"#.into(),body_bytes:Vec::new()});
        }
        self.inner.request(request).await
    }
    async fn stream_sse(&self, request: HttpRequest) -> Result<SseStream, HttpError> {
        self.inner.stream_sse(request).await
    }
}

fn fixture(
    http: Arc<dyn HttpTransport>,
) -> (
    Arc<McpRegistry>,
    Arc<RecordingTransport>,
    Arc<MemStorage>,
    Arc<AtomicUsize>,
) {
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let browser = Arc::new(AtomicUsize::new(0));
    let opened = browser.clone();
    let registry = Arc::new(McpRegistry::new(transport.clone()).with_oauth(OAuthDeps {
        http,
        clock: TestClock::new(1000),
        storage: storage.clone(),
        on_authorization_url: Arc::new(move |_| {
            opened.fetch_add(1, Ordering::SeqCst);
        }),
        xaa_config: None,
    }));
    (registry, transport, storage, browser)
}
fn good_as() -> Arc<MockAs> {
    MockAs::new(
        r#"{"access_token":"sdk-access","refresh_token":"sdk-refresh","expires_in":3600,"token_type":"Bearer"}"#,
        r#"{"access_token":"sdk-refreshed","token_type":"Bearer"}"#,
    )
}
async fn seed(registry: &McpRegistry, config: &McpServerConfig) {
    registry.connections.write().await.insert(
        config.name.clone(),
        McpConnectionState::NeedsAuth {
            config: config.clone(),
            error: "fixture needs auth".into(),
        },
    );
}
fn callback(ready: &Value) -> String {
    let url = url::Url::parse(ready["authUrl"].as_str().unwrap()).unwrap();
    let pairs = url.query_pairs().collect::<HashMap<_, _>>();
    format!(
        "{}?code=sdk-code&state={}",
        pairs["redirect_uri"], pairs["state"]
    )
}
async fn port_closes(port: u16) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("owned callback listener must close");
}

#[tokio::test]
async fn native_322_sdk_authentication_returns_url_then_validates_callback_and_reuses_grant() {
    for sse in [false, true] {
        let peer = good_as();
        let (registry, transport, storage, browser) = fixture(peer.clone());
        let mut config = http_cfg("oauth raw Ω", None);
        if sse {
            if let McpTransportSpec::Http {
                url,
                headers,
                headers_helper,
                oauth,
            } = config.spec
            {
                config.spec = McpTransportSpec::Sse {
                    url,
                    headers,
                    headers_helper,
                    oauth,
                };
            }
        }
        seed(&registry, &config).await;
        let name = config.authored_name_units();
        let ready = registry
            .start_sdk_authentication(&name, None, false)
            .await
            .unwrap();
        assert_eq!(ready["requiresUserAction"], true);
        assert_eq!(ready["callbackExpected"], true);
        assert_eq!(ready["redirectScheme"], "localhost");
        let port = ready["callbackPort"].as_u64().unwrap() as u16;
        assert_eq!(
            browser.load(Ordering::SeqCst),
            0,
            "SDK auth never opens a browser"
        );
        assert!(!peer
            .requests()
            .iter()
            .any(|request| request.url.ends_with("/token")));
        assert_eq!(registry.submit_sdk_oauth_callback(&name,"lingxi-test://callback").await.unwrap_err().message,"Invalid callback URL: missing authorization code. Please paste the full redirect URL including the code parameter.");
        assert_eq!(registry.submit_sdk_oauth_callback(&name,"lingxi-test://callback?code=x&state=wrong").await.unwrap_err().message,"Callback URL not accepted: it belongs to a different sign-in attempt (OAuth state mismatch) or carries no authorization code. The sign-in in progress is still waiting; submit the redirect URL from its own browser page.");
        registry
            .submit_sdk_oauth_callback(&name, &callback(&ready))
            .await
            .unwrap();
        assert_eq!(
            transport.connect_count(),
            0,
            "pasted-callback ACK must not reconnect"
        );
        let key = oauth::server_key(config.storage_name(), &config.spec);
        let saved = oauth::load_tokens(&(storage.clone() as Arc<dyn SecureStorage>), &key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.access_token, "sdk-access");
        port_closes(port).await;
        registry
            .reconnect_preserving_auth(&config.name)
            .await
            .unwrap();
        assert_eq!(
            spec_auth_header(&transport.last_spec()).as_deref(),
            Some("Bearer sdk-access")
        );
        assert_eq!(browser.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn native_322_sdk_authentication_custom_redirect_fallback_and_error_code() {
    for (only, reject, success) in [
        (true, false, true),
        (false, true, true),
        (true, true, false),
    ] {
        let peer = good_as();
        let (registry, _, _, browser) = fixture(Arc::new(SdkAs {
            inner: peer.clone(),
            reject_custom: reject,
            reject_all: false,
        }));
        let config = http_cfg("custom raw Ω", None);
        seed(&registry, &config).await;
        let name = config.authored_name_units();
        let result = registry
            .start_sdk_authentication(&name, Some("lingxi-test://oauth/callback".into()), only)
            .await;
        if success {
            let ready = result.unwrap();
            assert_eq!(
                ready["redirectScheme"],
                if reject { "localhost" } else { "custom" }
            );
            assert_eq!(ready.get("callbackPort").is_some(), reject);
            registry
                .submit_sdk_oauth_callback(&name, &callback(&ready))
                .await
                .unwrap();
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.message,"SDK auth failed: Dynamic Client Registration rejected (HTTP 400): { error : invalid_redirect_uri , error_description : LOCAL_REDIRECT_REJECTED }");
            assert_eq!(
                error.error_code.as_deref(),
                Some("mcp_redirect_uri_unusable")
            );
        }
        assert_eq!(browser.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn native_322_sdk_authentication_browser_completion_reconnects_and_token_error_is_exact() {
    let (registry, transport, _, browser) = fixture(good_as());
    let mut config = http_cfg("private-browser-handle", None);
    config.metadata.authored_name = Some(mcp::McpServerName::from_utf16(vec![
        0x62, 0x72, 0x6f, 0x77, 0x73, 0x65, 0x72, 0xd800,
    ]));
    seed(&registry, &config).await;
    let mut shadow = http_cfg("scalar-shadow-key", None);
    shadow.metadata.authored_name = Some(config.name.as_str().into());
    seed(&registry, &shadow).await;
    assert_eq!(
        registry.get_config(&config.name).await.unwrap().name,
        shadow.name,
        "public lookup intentionally resolves the authored scalar shadow"
    );
    let ready = registry
        .start_sdk_authentication(&config.authored_name_units(), None, false)
        .await
        .unwrap();
    drive_browser(ready["authUrl"].as_str().unwrap()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while transport.connect_count() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer sdk-access")
    );
    assert_eq!(browser.load(Ordering::SeqCst), 0);
    let peer = MockAs::new(
        r#"{"error":"invalid_grant","error_description":"LOCAL_GRANT_REJECTED"}"#,
        "",
    );
    let (registry, transport, _, _) = fixture(peer);
    let config = http_cfg("denied", None);
    seed(&registry, &config).await;
    let name = config.authored_name_units();
    let ready = registry
        .start_sdk_authentication(&name, None, false)
        .await
        .unwrap();
    assert_eq!(
        registry
            .submit_sdk_oauth_callback(&name, &callback(&ready))
            .await
            .unwrap_err()
            .message,
        "LOCAL_GRANT_REJECTED"
    );
    assert_eq!(transport.connect_count(), 0);
}

#[tokio::test]
async fn native_322_sdk_authentication_cancels_on_shutdown_reconfiguration_and_owner_drop() {
    for mode in ["shutdown", "reconfigure", "supersede", "drop"] {
        let peer = good_as();
        let (registry, _, _, _) = fixture(peer.clone());
        let config = http_cfg("cancel", None);
        seed(&registry, &config).await;
        let name = config.authored_name_units();
        let ready = registry
            .start_sdk_authentication(&name, None, false)
            .await
            .unwrap();
        let port = ready["callbackPort"].as_u64().unwrap() as u16;
        let weak = Arc::downgrade(&registry);
        match mode {
            "shutdown" => registry.cancel_sdk_authentications(),
            "reconfigure" => {
                let mut replacement = config.clone();
                if let McpTransportSpec::Http { url, .. } = &mut replacement.spec {
                    *url = "https://replacement.example/mcp".into();
                }
                registry.connect(replacement).await.unwrap();
            }
            "supersede" => {
                let next = registry
                    .start_sdk_authentication(&name, None, false)
                    .await
                    .unwrap();
                port_closes(port).await;
                assert!(registry
                    .submit_sdk_oauth_callback(&name, &callback(&ready))
                    .await
                    .unwrap_err()
                    .message
                    .starts_with("Callback URL not accepted"));
                registry
                    .submit_sdk_oauth_callback(&name, &callback(&next))
                    .await
                    .unwrap();
            }
            _ => {}
        }
        if mode != "drop" {
            assert_eq!(
                registry
                    .submit_sdk_oauth_callback(&name, &callback(&ready))
                    .await
                    .unwrap_err()
                    .message,
                "No active OAuth flow for server: cancel"
            );
        }
        drop(registry);
        assert!(
            weak.upgrade().is_none(),
            "OAuth task must not retain its registry"
        );
        port_closes(port).await;
        assert_eq!(
            peer.requests()
                .iter()
                .filter(|request| request.url.ends_with("/token"))
                .count(),
            usize::from(mode == "supersede")
        );
    }
}

#[tokio::test]
async fn native_322_sdk_authentication_preserves_raw_credential_namespace_and_unsupported_diagnostics(
) {
    let (registry, _, storage, _) = fixture(good_as());
    let mut config = http_cfg("private-raw", None);
    let name = vec![0x72, 0x61, 0x77, 0xd800];
    config.metadata.authored_name = Some(mcp::McpServerName::from_utf16(name.clone()));
    seed(&registry, &config).await;
    let ready = registry
        .start_sdk_authentication(&name, None, false)
        .await
        .unwrap();
    assert!(registry
        .submit_sdk_oauth_callback(
            &"raw�".encode_utf16().collect::<Vec<_>>(),
            &callback(&ready)
        )
        .await
        .unwrap_err()
        .message
        .starts_with("No active OAuth flow"));
    registry
        .submit_sdk_oauth_callback(&name, &callback(&ready))
        .await
        .unwrap();
    let key = oauth::server_key(config.storage_name(), &config.spec);
    assert!(storage
        .map
        .lock()
        .unwrap()
        .contains_key(&("mcp-utf16-mcp-oauth".into(), key.clone())));
    assert!(!storage
        .map
        .lock()
        .unwrap()
        .contains_key(&("mcp-oauth".into(), key)));
    let mut sdk = http_cfg("sdk raw Ω", None);
    sdk.spec = McpTransportSpec::SdkControl {
        control_channel_id: "sdk raw Ω".into(),
    };
    seed(&registry, &sdk).await;
    assert_eq!(
        registry
            .start_sdk_authentication(&sdk.authored_name_units(), None, false)
            .await
            .unwrap_err()
            .message,
        "Server type \"sdk\" does not support OAuth authentication"
    );
    let missing = "missing raw Ω".encode_utf16().collect::<Vec<_>>();
    assert_eq!(
        registry
            .start_sdk_authentication(&missing, None, false)
            .await
            .unwrap_err()
            .message,
        "Server not found: missing raw Ω"
    );
    let peer = good_as();
    let (registry, _, _, browser) = fixture(peer.clone());
    let mut proxy = http_cfg("cloud connector", None);
    proxy.metadata.transport = Some("claudeai-proxy".into());
    seed(&registry, &proxy).await;
    assert_eq!(
        registry
            .start_sdk_authentication(&proxy.authored_name_units(), None, false)
            .await
            .unwrap_err()
            .message,
        "Unable to build claude.ai connector auth URL (missing org or server id)"
    );
    assert!(
        peer.requests().is_empty(),
        "connector context must be checked before discovery or revocation"
    );
    assert_eq!(browser.load(Ordering::SeqCst), 0);
}
