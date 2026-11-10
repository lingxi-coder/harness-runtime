use super::*;
use crate::oauth;
use lingxi_core::host::secure_storage::InMemorySecureStorage;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1000)
    }
}

#[test]
fn registration_binding_preserves_utf16_and_redirect_origin_rules() {
    let identity = McpOAuthIdentity::default();
    let mut state = OAuthSessionState::default();
    let raw = r#"{"client_id":"fake\ud800","client_secret":"fake\udfff","unknown":{"\ud800":1}}"#;
    state.client_registration = Some(raw.into());
    state.client_issuer = Some(vec![0xd800]);
    state.redirect_uri = Some("http://localhost:1234/callback".into());
    assert_eq!(saved_callback_port(&state), Some(1234));
    assert!(state
        .resolve_client(
            None,
            &[0xfffd],
            Some("http://localhost:5678/callback"),
            &identity
        )
        .is_none());
    let client = state
        .resolve_client(
            None,
            &[0xd800],
            Some("http://localhost:5678/callback"),
            &identity,
        )
        .unwrap();
    assert_eq!(client.raw, raw);
    assert!(state
        .resolve_client(
            None,
            &[0xd800],
            Some("http://127.0.0.1:5678/callback"),
            &identity
        )
        .is_none());
    assert!(issuer_equal(
        &"https://as.example/".encode_utf16().collect::<Vec<_>>(),
        &"https://as.example".encode_utf16().collect::<Vec<_>>()
    ));
    state.invalidate("discovery");
    assert_eq!(state.client_registration.as_deref(), Some(raw));
    state.invalidate("tokens");
    assert_eq!(state.client_registration.as_deref(), Some(raw));
    state.invalidate("client");
    assert!(state.client_registration.is_none());
    assert!(saved_callback_port(&state).is_none());
}

#[test]
fn issuer_echo_rollout_preserves_observe_and_same_origin_rules() {
    for received in [
        "https://as.example",
        "https://as.example/",
        "https://as.example/tenant",
    ] {
        assert!(oauth::validate_issuer_echo("https://as.example", Some(received), true).is_ok());
    }
    for received in [
        "https://other.example",
        "https://as.example?query=1",
        "https://as.example#fragment",
        "https://user@as.example",
        "not a URL",
    ] {
        assert!(oauth::validate_issuer_echo("https://as.example", Some(received), false).is_ok());
        assert!(oauth::validate_issuer_echo("https://as.example", Some(received), true).is_err());
    }
}

#[test]
fn manual_callback_retries_wrong_flow_and_uses_first_values() {
    for value in [
        "not a URL",
        "fake-code",
        "http://localhost/callback?state=S",
        "http://localhost/callback?error=denied&state=WRONG",
    ] {
        assert!(oauth::try_manual_callback_url(value, "S", None, false).is_none());
    }
    assert_eq!(
        oauth::try_manual_callback_url(
            "http://localhost/callback?code=first&code=second&state=S",
            "S",
            None,
            false
        )
        .unwrap()
        .unwrap(),
        "first"
    );
    assert!(
        matches!(oauth::try_manual_callback_url("http://localhost/callback?error=denied&error_description=fake&state=S","S",None,false),Some(Err(oauth::OAuthError::Callback(message))) if message=="OAuth error: denied - fake")
    );
}

#[test]
fn expiry_storage_uses_javascript_number_bytes() {
    for (number, expected) in [
        (4_600_000.0, "4600000"),
        (1e20, "100000000000000000000"),
        (1e21, "1e+21"),
        (-0.0, "0"),
        (f64::INFINITY, "null"),
        (f64::NAN, "null"),
    ] {
        assert_eq!(
            serde_json::to_string(&oauth::OAuthExpiry::Number(number)).unwrap(),
            expected
        );
    }
}

struct FixtureAuth {
    cimd: bool,
    requests: Mutex<Vec<HttpRequest>>,
    registrations: AtomicUsize,
}
#[async_trait::async_trait]
impl HttpTransport for FixtureAuth {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let url = url::Url::parse(&request.url).unwrap();
        let body = request.body.clone().unwrap_or_default();
        self.requests.lock().unwrap().push(request.clone());
        let value = if url.path().contains("oauth-protected-resource") {
            serde_json::json!({"resource":"https://mcp.example/mcp","authorization_servers":["https://as.example"],"scopes_supported":["read"]})
        } else if url.path().contains(".well-known") {
            serde_json::json!({"issuer":"https://as.example","authorization_endpoint":"https://as.example/authorize","token_endpoint":"https://as.example/token","registration_endpoint":"https://as.example/register","response_types_supported":["code"],"token_endpoint_auth_methods_supported":["client_secret_post","none"],"client_id_metadata_document_supported":self.cimd})
        } else if url.path() == "/register" {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            let mut value: serde_json::Value = serde_json::from_str(&body).unwrap();
            value["client_id"] = serde_json::json!("fake-registered");
            value["client_secret"] = serde_json::json!("fake +~secret");
            value["token_endpoint_auth_method"] = serde_json::json!("client_secret_post");
            value
        } else if url.path() == "/token" {
            serde_json::json!({"access_token":"fake-access","refresh_token":"fake-refresh","token_type":"Bearer","expires_in":3600})
        } else {
            panic!("unexpected fixture path {}", url.path())
        };
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: value.to_string(),
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        unreachable!()
    }
}
fn authorization_callback() -> oauth::OnAuthorizationUrl {
    Arc::new(|url| {
        let url = url::Url::parse(url).unwrap();
        let query = url
            .query_pairs()
            .into_owned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
        let state = query["state"].clone();
        tokio::spawn(async move {
            let mut socket =
                tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
                    .await
                    .unwrap();
            let query = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([("code", "fake-code"), ("state", state.as_str())])
                .finish();
            socket
                .write_all(
                    format!("GET /callback?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            assert!(String::from_utf8_lossy(&response).contains("200 OK"));
        });
    })
}
#[tokio::test]
async fn stored_registration_secret_and_cimd_dcr_fallback_reach_real_callback() {
    for (cimd, metadata_identity, expected_registrations, client_id) in [
        (false, None, 1, "fake-registered"),
        (
            true,
            Some("https://lingxi.example/mcp-client.json"),
            0,
            "https://lingxi.example/mcp-client.json",
        ),
        (
            false,
            Some("https://lingxi.example/mcp-client.json"),
            1,
            "fake-registered",
        ),
    ] {
        let peer = Arc::new(FixtureAuth {
            cimd,
            requests: Mutex::new(Vec::new()),
            registrations: AtomicUsize::new(0),
        });
        let http: Arc<dyn HttpTransport> = peer.clone();
        let storage: Arc<dyn SecureStorage> = Arc::new(InMemorySecureStorage::new());
        let clock: Arc<dyn Clock> = Arc::new(FixedClock);
        let identity = McpOAuthIdentity {
            client_name: "LingXi Fixture".into(),
            client_metadata_url: metadata_identity.map(str::to_owned),
            refresh_lock_dir: None,
            strict_issuer_echo: false,
        };
        let config = lingxi_core::host::McpOAuthConfigDto {
            client_id: None,
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        };
        for _ in 0..2 {
            let tokens = tokio::time::timeout(
                Duration::from_secs(2),
                oauth::perform_oauth_flow_stored(
                    &http,
                    &clock,
                    &config,
                    "fake-server",
                    "https://mcp.example/mcp",
                    &authorization_callback(),
                    None,
                    None,
                    None,
                    &storage,
                    "fake-key",
                    &identity,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(tokens.client_id.as_deref(), Some(client_id));
            oauth::save_tokens(&storage, &clock, "fake-key", &tokens)
                .await
                .unwrap();
        }
        assert_eq!(
            peer.registrations.load(Ordering::SeqCst),
            expected_registrations
        );
        let state = load(&storage, "fake-key").await.unwrap();
        assert_eq!(
            state.client_issuer,
            Some("https://as.example".encode_utf16().collect())
        );
        assert!(state
            .token_response
            .as_deref()
            .unwrap()
            .contains("fake-refresh"));
        assert!(
            state
                .discovery
                .as_ref()
                .unwrap()
                .authorization_metadata
                .is_none(),
            "persist URL binding while retaining in-flow metadata separately"
        );
        let requests = peer.requests.lock().unwrap();
        let grants = requests
            .iter()
            .filter(|request| request.url.ends_with("/token"))
            .collect::<Vec<_>>();
        assert_eq!(grants.len(), 2);
        for grant in grants {
            let fields = url::form_urlencoded::parse(grant.body.as_deref().unwrap().as_bytes())
                .into_owned()
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(fields["client_id"], client_id);
            if expected_registrations > 0 {
                assert_eq!(fields["client_secret"], "fake +~secret");
            }
        }
        if expected_registrations > 0 {
            let request = requests
                .iter()
                .find(|request| request.url.ends_with("/register"))
                .unwrap();
            assert!(request
                .body
                .as_deref()
                .unwrap()
                .contains("LingXi Fixture (fake-server)"));
            assert_eq!(
                oauth::load_tokens(&storage, "fake-key")
                    .await
                    .unwrap()
                    .unwrap()
                    .client_secret
                    .as_deref(),
                Some("fake +~secret")
            );
        }
        drop(requests);
        clear_auth(&storage, "fake-key").await.unwrap();
        assert!(load(&storage, "fake-key")
            .await
            .unwrap()
            .client_registration
            .is_none());
    }
}

#[tokio::test]
async fn refresh_lease_is_exclusive_stale_recoverable_and_cancel_safe() {
    let root = tempfile::tempdir().unwrap();
    let identity = McpOAuthIdentity {
        refresh_lock_dir: Some(root.path().to_owned()),
        ..McpOAuthIdentity::default()
    };
    let lease = RefreshLock::acquire(&identity, "fake|key-1").await.unwrap();
    let path = root.path().join("mcp-refresh-fake_key_1.lock.lock");
    assert!(path.is_dir());
    let competing = identity.clone();
    let blocked = tokio::spawn(async move { RefreshLock::acquire(&competing, "fake|key-1").await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!blocked.is_finished());
    blocked.abort();
    let _ = blocked.await;
    drop(lease);
    assert!(!path.exists());
    std::fs::create_dir(&path).unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(61))
        .unwrap();
    let lease = RefreshLock::acquire(&identity, "fake|key-1").await.unwrap();
    drop(lease);
    assert!(!path.exists());
}

#[tokio::test]
async fn refresh_lease_child_process() {
    let Some(root) = std::env::var_os("LINGXI_MCP_TEST_LOCK_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    let identity = McpOAuthIdentity {
        refresh_lock_dir: Some(root.clone()),
        ..McpOAuthIdentity::default()
    };
    let lease = RefreshLock::acquire(&identity, "fake-process")
        .await
        .unwrap();
    std::fs::write(root.join("ready"), b"fake-child-ready").unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    drop(lease);
}

#[tokio::test]
async fn refresh_lease_serializes_independent_processes() {
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "oauth::session::tests::refresh_lease_child_process",
            "--nocapture",
        ])
        .env("LINGXI_MCP_TEST_LOCK_CHILD", root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !root.path().join("ready").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not acquire fixture lock"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before fixture lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let identity = McpOAuthIdentity {
        refresh_lock_dir: Some(root.path().to_owned()),
        ..McpOAuthIdentity::default()
    };
    let started = std::time::Instant::now();
    let lease = RefreshLock::acquire(&identity, "fake-process")
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "processes must not hold the same refresh lease concurrently"
    );
    assert!(child.wait().unwrap().success());
    drop(lease);
}

struct ScriptedHttp {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<VecDeque<HttpResponse>>,
}
#[async_trait::async_trait]
impl HttpTransport for ScriptedHttp {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request);
        Ok(self.responses.lock().unwrap().pop_front().unwrap())
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        unreachable!()
    }
}
fn response(status: u16, location: Option<&str>) -> HttpResponse {
    HttpResponse {
        status,
        headers: location
            .map(|value| vec![("location".into(), value.into())])
            .unwrap_or_default(),
        body: String::new(),
        body_bytes: Vec::new(),
    }
}
#[tokio::test]
async fn configured_headers_and_redirects_obey_each_oauth_origin_boundary() {
    let spec = McpTransportSpec::Http {
        url: "https://mcp.example/mcp".into(),
        headers: [
            ("X-Fake-Key".into(), "fake-key".into()),
            ("Authorization".into(), "Bearer must-not-forward".into()),
        ]
        .into_iter()
        .collect(),
        headers_helper: None,
        oauth: None,
    };
    let peer = Arc::new(ScriptedHttp {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(
            [
                response(302, Some("https://as.example/metadata")),
                response(200, None),
            ]
            .into_iter()
            .collect(),
        ),
    });
    let inner: Arc<dyn HttpTransport> = peer.clone();
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: "https://mcp.example/metadata".into(),
        headers: vec![("accept".into(), "application/json".into())],
        body: None,
        body_bytes: None,
        timeout: None,
    };
    configured_http(&inner, &spec)
        .request(request.clone())
        .await
        .unwrap();
    let seen = peer.requests.lock().unwrap();
    assert!(seen[0]
        .headers
        .iter()
        .any(|(key, value)| key == "x-fake-key" && value == "fake-key"));
    assert!(!seen[0]
        .headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("authorization")));
    assert!(!seen[1]
        .headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("x-fake-key")));
    drop(seen);
    for revocation in [false, true] {
        let peer = Arc::new(ScriptedHttp {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(
                [response(307, Some("https://other.example/token"))]
                    .into_iter()
                    .collect(),
            ),
        });
        let inner: Arc<dyn HttpTransport> = peer.clone();
        let mut request = request.clone();
        request.method = HttpMethod::Post;
        request.body = Some("token=fake".into());
        let http = if revocation {
            configured_revocation_http(&inner, &spec)
        } else {
            configured_http(&inner, &spec)
        };
        let result = http.request(request).await;
        assert_eq!(result.is_err(), revocation);
        assert_eq!(peer.requests.lock().unwrap().len(), 1);
    }
}
