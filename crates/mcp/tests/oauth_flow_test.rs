//! End-to-end OAuth 2.1 + PKCE flow for remote (SSE/HTTP) MCP servers, against
//! a local mock authorization server.
//!
//! Covers (task TDD list):
//! - discovery → DCR → PKCE → authorize URL → loopback callback → token
//!   exchange, and that the resulting connection attaches `Authorization:
//!   Bearer <access>` to the SSE/HTTP spec;
//! - a static-token server (`oauth: None`) is connected with its spec
//!   UNCHANGED (regression guard);
//! - refresh-on-expiry: a stored, expired token + refresh token drives a
//!   `refresh_token` grant and attaches the new access token;
//! - 401-on-connect: the transport 401s once, the registry refreshes and
//!   retries, and the retried spec carries the new Bearer.

use async_trait::async_trait;
use lingxi_core::host::http::SseStream;
use lingxi_core::host::{
    Clock, ElicitRequestDto, ElicitResultDto, HttpError, HttpTransport, McpError,
    McpNotificationStream, McpOAuthConfigDto, McpPromptDto, McpRawConnection,
    McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto, McpTransport,
    McpTransportKind, McpTransportSpec, SecureStorage, SecureStorageBackend, SecureStorageError,
    ServerCapabilitiesDto,
};
use lingxi_core::types::{
    HttpMethod, HttpRequest, HttpResponse, SecretKindDto, SecureStorageData, SecureStorageMetadata,
};
use mcp::connection::{ConfigScope, McpServerConfig};
use mcp::oauth::{self, OnAuthorizationUrl};
use mcp::registry::{McpRegistry, OAuthDeps};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Mock authorization server (HTTP transport with canned, body-aware routes).
// ---------------------------------------------------------------------------

/// A canned HTTP response.
#[derive(Clone)]
struct Canned {
    status: u16,
    body: String,
}

/// HTTP transport that fakes the authorization server:
/// - GET `.well-known/oauth-authorization-server` → AS metadata
/// - GET `.well-known/oauth-protected-resource`   → 404 (force the 8414 path)
/// - POST `/register`                             → DCR (issues `client_id`)
/// - POST `/token` with `grant_type=authorization_code` → first token set
/// - POST `/token` with `grant_type=refresh_token`      → refreshed token set
struct MockAs {
    requests: Mutex<Vec<HttpRequest>>,
    exchange_body: String,
    refresh_body: String,
}

impl MockAs {
    fn new(exchange_body: &str, refresh_body: &str) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            exchange_body: exchange_body.into(),
            refresh_body: refresh_body.into(),
        })
    }

    fn route(&self, req: &HttpRequest) -> Canned {
        let url = &req.url;
        let body = req.body.as_deref().unwrap_or("");
        if url.contains("oauth-protected-resource") {
            // Force the RFC 8414 direct-against-server fallback.
            return Canned {
                status: 404,
                body: String::new(),
            };
        }
        if url.contains("oauth-authorization-server") {
            return Canned {
                status: 200,
                body: r#"{
                    "authorization_endpoint": "https://as.example.com/authorize",
                    "token_endpoint": "https://as.example.com/token",
                    "registration_endpoint": "https://as.example.com/register",
                    "scopes_supported": ["mcp:read"],
                    "revocation_endpoint": "https://as.example.com/revoke"
                }"#
                .into(),
            };
        }
        if url.contains("/revoke") {
            return Canned {
                status: 200,
                body: String::new(),
            };
        }
        if url.contains("/register") {
            return Canned {
                status: 201,
                body: r#"{"client_id": "dyn-client-9"}"#.into(),
            };
        }
        if url.contains("/token") {
            if body.contains("grant_type=refresh_token") {
                return Canned {
                    status: 200,
                    body: self.refresh_body.clone(),
                };
            }
            return Canned {
                status: 200,
                body: self.exchange_body.clone(),
            };
        }
        Canned {
            status: 404,
            body: String::new(),
        }
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for MockAs {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let canned = self.route(&req);
        self.requests.lock().unwrap().push(req);
        Ok(HttpResponse {
            status: canned.status,
            headers: vec![],
            body: canned.body,
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
        Err(HttpError::InvalidRequest("sse unused".into()))
    }
}

// ---------------------------------------------------------------------------
// Fixed clock + in-memory secure storage.
// ---------------------------------------------------------------------------

struct TestClock {
    secs: AtomicU64,
}
impl TestClock {
    fn new(secs: u64) -> Arc<Self> {
        Arc::new(Self {
            secs: AtomicU64::new(secs),
        })
    }
}
impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.secs.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct MemStorage {
    map: Mutex<HashMap<(String, String), SecureStorageData>>,
}
impl MemStorage {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}
#[async_trait]
impl SecureStorage for MemStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .insert((service.into(), account.into()), data);
        Ok(())
    }
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&(service.into(), account.into()))
            .cloned())
    }
    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .remove(&(service.into(), account.into()));
        Ok(())
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, a)| a.clone())
            .collect())
    }
    fn is_encrypted(&self) -> bool {
        false
    }
    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::PlainText
    }
}

// ---------------------------------------------------------------------------
// Recording MCP transport: captures the spec it was connected with, and can be
// configured to 401 the first N connect attempts.
// ---------------------------------------------------------------------------

struct RecordingTransport {
    seen_specs: Mutex<Vec<McpTransportSpec>>,
    fail_401_first: AtomicUsize,
    /// When >0, the first N connects return a 403 `insufficient_scope` error
    /// carrying the configured required scope (step-up path).
    fail_403_first: AtomicUsize,
    fail_403_scope: Mutex<String>,
    /// §24c: when set, the 401 failure carries this exact `WWW-Authenticate`
    /// value; otherwise it is the same structured `McpError::HttpResponse`
    /// with no challenge header.
    structured_401_challenge: Mutex<Option<String>>,
}
impl RecordingTransport {
    fn new(fail_401_first: usize) -> Arc<Self> {
        Arc::new(Self {
            seen_specs: Mutex::new(Vec::new()),
            fail_401_first: AtomicUsize::new(fail_401_first),
            fail_403_first: AtomicUsize::new(0),
            fail_403_scope: Mutex::new(String::new()),
            structured_401_challenge: Mutex::new(None),
        })
    }
    /// Build a transport whose first connect returns a 403 `insufficient_scope`
    /// requiring `scope`, then succeeds.
    fn with_403_first(scope: &str) -> Arc<Self> {
        Arc::new(Self {
            seen_specs: Mutex::new(Vec::new()),
            fail_401_first: AtomicUsize::new(0),
            fail_403_first: AtomicUsize::new(1),
            fail_403_scope: Mutex::new(scope.into()),
            structured_401_challenge: Mutex::new(None),
        })
    }
    /// Make the next `fail_401_first` 401(s) carry `challenge` as a structured
    /// `WWW-Authenticate` value rather than the default flattened string.
    fn with_401_challenge(self: Arc<Self>, challenge: &str) -> Arc<Self> {
        *self.structured_401_challenge.lock().unwrap() = Some(challenge.to_string());
        self
    }
    fn last_spec(&self) -> McpTransportSpec {
        self.seen_specs.lock().unwrap().last().cloned().unwrap()
    }
    fn connect_count(&self) -> usize {
        self.seen_specs.lock().unwrap().len()
    }
}

/// Pull `Authorization` out of an SSE/HTTP spec's headers, if present.
fn spec_auth_header(spec: &McpTransportSpec) -> Option<String> {
    match spec {
        McpTransportSpec::Sse { headers, .. } | McpTransportSpec::Http { headers, .. } => {
            headers.get("Authorization").cloned()
        }
        _ => None,
    }
}

#[async_trait]
impl McpTransport for RecordingTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        self.seen_specs.lock().unwrap().push(spec.clone());
        if self.fail_403_first.load(Ordering::SeqCst) > 0 {
            self.fail_403_first.fetch_sub(1, Ordering::SeqCst);
            let scope = self.fail_403_scope.lock().unwrap().clone();
            // Flattened transport error mirroring a 403 WWW-Authenticate body.
            return Err(McpError::Connection(format!(
                "HTTP 403 Forbidden: Bearer error=\"insufficient_scope\", scope=\"{scope}\""
            )));
        }
        if self.fail_401_first.load(Ordering::SeqCst) > 0 {
            self.fail_401_first.fetch_sub(1, Ordering::SeqCst);
            if let Some(challenge) = self.structured_401_challenge.lock().unwrap().clone() {
                return Err(McpError::HttpResponse {
                    status: 401,
                    www_authenticate: Some(challenge),
                });
            }
            // Every wired transport reports a real 401 STRUCTURALLY —
            // `SseConnectError::HttpResponse` for SSE's pre-flight GET
            // (platforms/common/src/mcp_sse.rs) and `handshake_error`'s
            // `data:{httpStatus,wwwAuthenticate}` unwrap for Streamable HTTP
            // (platforms/posix/src/mcp.rs) — so the mock must too. A
            // flattened `Connection("HTTP 401 …")` string is not a shape any
            // production path can still produce, and pretending otherwise
            // hid that `classify_auth_failure`'s gate was substring-based.
            return Err(McpError::HttpResponse {
                status: 401,
                www_authenticate: None,
            });
        }
        Ok(McpRawConnection {
            connection_id: lingxi_core::types::McpConnectionId::new(),
        })
    }
    async fn initialize(&self, _c: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        })
    }
    async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        Ok(vec![])
    }
    async fn list_resources(&self, _c: &McpRawConnection) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(vec![])
    }
    async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        Ok(vec![])
    }
    async fn call_tool(
        &self,
        _c: &McpRawConnection,
        _t: &str,
        _i: serde_json::Value,
    ) -> Result<McpToolResultDto, McpError> {
        Ok(McpToolResultDto::default())
    }
    async fn read_resource(
        &self,
        _c: &McpRawConnection,
        _u: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        Err(McpError::Internal("unused".into()))
    }
    async fn ping(&self, _id: lingxi_core::types::McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }
    async fn notifications(
        &self,
        _c: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!("notifications unused")
    }
    async fn handle_elicitation(
        &self,
        _c: &McpRawConnection,
        _r: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Err(McpError::Internal("unused".into()))
    }
    async fn disconnect(&self, _id: lingxi_core::types::McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Http]
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn http_cfg(name: &str, oauth: Option<McpOAuthConfigDto>) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth,
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

fn oauth_block(client_id: Option<&str>) -> McpOAuthConfigDto {
    McpOAuthConfigDto {
        client_id: client_id.map(str::to_string),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    }
}

/// Fire a fake-browser GET to the loopback `/callback` once `auth_url` is
/// surfaced. Extracts the redirect port + `state` from the URL.
async fn drive_browser(auth_url: &str) {
    // redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcallback
    let port = auth_url
        .split("localhost%3A")
        .nth(1)
        .and_then(|s| s.split("%2Fcallback").next())
        .expect("redirect port in auth url")
        .to_string();
    let state = auth_url
        .split("state=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .expect("state in auth url")
        .to_string();
    // Retry the connect briefly until the listener is bound.
    for _ in 0..50 {
        if let Ok(mut s) =
            tokio::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).await
        {
            use tokio::io::AsyncWriteExt;
            let req = format!(
                "GET /callback?code=auth-code-xyz&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            let _ = s.write_all(req.as_bytes()).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("callback listener never bound on port {port}");
}

/// `on_authorization_url` hook that pushes the surfaced URL onto a channel so
/// the test can drive the fake browser.
fn url_capture() -> (
    OnAuthorizationUrl,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let cb: OnAuthorizationUrl = Arc::new(move |url: &str| {
        let _ = tx.send(url.to_string());
    });
    (cb, rx)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_flow_attaches_bearer_and_persists_tokens() {
    let exchange = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}"#;
    let mock_as = MockAs::new(exchange, "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, mut url_rx) = url_capture();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock.clone() as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    // No client_id configured → DCR is exercised.
    let config = http_cfg("remote", Some(oauth_block(None)));

    // Drive the browser when the auth URL is surfaced, concurrently with connect.
    let browser = tokio::spawn(async move {
        let url = url_rx.recv().await.expect("auth url surfaced");
        drive_browser(&url).await;
    });

    let registry2 = Arc::new(registry);
    registry2.connect(config.clone()).await.expect("connect ok");
    browser.await.unwrap();

    // The transport saw the Bearer-augmented spec.
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer access-1")
    );

    // Discovery hit oauth-authorization-server; DCR hit /register; token
    // exchange used the authorization_code grant with the PKCE verifier.
    let reqs = mock_as.requests();
    assert!(reqs
        .iter()
        .any(|r| r.url.contains("oauth-authorization-server")));
    let register = reqs.iter().find(|r| r.url.contains("/register")).unwrap();
    assert_eq!(register.method, HttpMethod::Post);
    // FIX 3: the DCR client metadata carries the per-server client_name
    // `LingXi (${serverName})` (auth.ts:1419).
    let reg_body = register.body.as_deref().unwrap();
    assert!(
        reg_body.contains("\"client_name\":\"LingXi (remote)\""),
        "DCR client_name should be per-server; body={reg_body}"
    );
    // cc 2.1.196 fix (getCuratedMetadataScope): with NO scope configured and NO
    // explicit authServerMetadataUrl, the request must NOT ask for the full
    // `scopes_supported` catalog — the DCR metadata carries no scope at all.
    assert!(
        !reg_body.contains("\"scope\""),
        "no-scope DCR must not request the scopes_supported catalog; body={reg_body}"
    );
    // FIX 1: the persisted tokens carry the DCR-issued client_id so refresh
    // re-sends it (asserted on storage below).
    let token = reqs.iter().find(|r| r.url.contains("/token")).unwrap();
    let body = token.body.as_deref().unwrap();
    assert!(body.contains("grant_type=authorization_code"));
    assert!(body.contains("code=auth-code-xyz"));
    assert!(body.contains("code_verifier="));
    assert!(body.contains("client_id=dyn-client-9"));

    // Tokens were persisted under the server key.
    let key = oauth::server_key("remote", &config.spec);
    let stored = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap()
        .expect("tokens stored");
    assert_eq!(stored.access_token, "access-1");
    assert_eq!(stored.refresh_token.as_deref(), Some("refresh-1"));
    // FIX 1: the DCR-issued client_id is persisted, so a later silent refresh
    // re-sends it instead of an empty string.
    assert_eq!(stored.client_id.as_deref(), Some("dyn-client-9"));
}

#[tokio::test]
async fn static_token_server_spec_is_unchanged() {
    // oauth: None → the OAuth seam, even when wired, must not touch the spec.
    let mock_as = MockAs::new("{}", "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(0);
    let (on_url, _rx) = url_capture();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    let mut headers = lingxi_core::host::McpHeaders::new();
    headers.insert("X-Static".to_string(), "preset".to_string());
    let config = McpServerConfig {
        name: "static".into(),
        spec: McpTransportSpec::Http {
            url: "https://static.example.com".into(),
            headers: headers.clone(),
            headers_helper: None,
            oauth: None,
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    };

    registry.connect(config.clone()).await.expect("connect ok");

    // Spec is passed through verbatim: no Authorization header added.
    let seen = transport.last_spec();
    assert!(
        spec_auth_header(&seen).is_none(),
        "no Bearer for static server"
    );
    if let McpTransportSpec::Http { headers: h, .. } = seen {
        assert_eq!(h.get("X-Static").map(String::as_str), Some("preset"));
        assert_eq!(h.len(), 1, "no extra headers injected");
    } else {
        panic!("expected Http spec");
    }
}

#[tokio::test]
async fn expired_token_triggers_refresh_and_attaches_new_bearer() {
    let refresh = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600}"#;
    let mock_as = MockAs::new("{}", refresh);
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(10_000);
    let (on_url, _rx) = url_capture();

    let config = http_cfg("refreshing", Some(oauth_block(Some("preset-client"))));
    let key = oauth::server_key("refreshing", &config.spec);

    // Seed an EXPIRED token (expires_at = 5_000 < now = 10_000) + refresh token
    // + a DCR-issued client_id that DIFFERS from the configured one, so the
    // refresh must prefer the persisted id (FIX 1).
    let stored = oauth::StoredTokens {
        access_token: "stale".into(),
        refresh_token: Some("refresh-1".into()),
        expires_at_unix: 5_000,
        client_id: Some("dcr-issued-7".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    let data = SecureStorageData::new(
        bytes,
        SecureStorageMetadata {
            created_at: SystemTime::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("mcp_oauth_tokens".into()),
        },
    );
    storage
        .store(oauth::MCP_OAUTH_SERVICE, &key, data)
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry.connect(config.clone()).await.expect("connect ok");

    // The refreshed access token is attached.
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer access-2")
    );
    // A refresh_token grant was sent (no interactive flow / no callback).
    let token = mock_as
        .requests()
        .into_iter()
        .find(|r| r.url.contains("/token"))
        .unwrap();
    let refresh_body = token.body.as_deref().unwrap();
    assert!(refresh_body.contains("grant_type=refresh_token"));
    // FIX 1: the refresh carries the PERSISTED (DCR-issued) client_id, not the
    // configured one and NOT an empty string.
    assert!(
        refresh_body.contains("client_id=dcr-issued-7"),
        "refresh must re-send the persisted DCR client_id; body={refresh_body}"
    );
    assert!(!refresh_body.contains("client_id=preset-client"));
    assert!(!refresh_body.contains("client_id=&") && !refresh_body.ends_with("client_id="));
    // New tokens persisted — including the client_id, so the NEXT refresh re-sends it.
    let reloaded = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.access_token, "access-2");
    assert_eq!(reloaded.client_id.as_deref(), Some("dcr-issued-7"));
}

#[tokio::test]
async fn connect_401_triggers_refresh_and_retry() {
    let refresh = r#"{"access_token":"access-3","refresh_token":"refresh-3","expires_in":3600}"#;
    let mock_as = MockAs::new("{}", refresh);
    // First connect attempt 401s; the second (post-refresh) succeeds.
    let transport = RecordingTransport::new(1);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let config = http_cfg("flaky", Some(oauth_block(Some("preset-client"))));
    let key = oauth::server_key("flaky", &config.spec);

    // Seed an UNEXPIRED token (so the first connect uses it and 401s) + refresh
    // + a persisted DCR client_id (FIX 1: the 401-reauth path re-sends it too).
    let stored = oauth::StoredTokens {
        access_token: "access-old".into(),
        refresh_token: Some("refresh-1".into()),
        expires_at_unix: 99_999,
        client_id: Some("dcr-issued-7".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    let data = SecureStorageData::new(
        bytes,
        SecureStorageMetadata {
            created_at: SystemTime::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("mcp_oauth_tokens".into()),
        },
    );
    storage
        .store(oauth::MCP_OAUTH_SERVICE, &key, data)
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry
        .connect(config)
        .await
        .expect("connect ok after retry");

    // Two connect attempts: stale Bearer (401), then refreshed Bearer.
    assert_eq!(transport.connect_count(), 2);
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer access-3")
    );
    assert!(mock_as.requests().into_iter().any(|r| {
        r.url.contains("/token")
            && r.body
                .as_deref()
                .map(|b| {
                    b.contains("grant_type=refresh_token")
                        // FIX 1: 401-reauth refresh re-sends the persisted client_id.
                        && b.contains("client_id=dcr-issued-7")
                })
                .unwrap_or(false)
    }));
}

// ---------------------------------------------------------------------------
// §24c: a live 401's `WWW-Authenticate` `resource_metadata` challenge drives
// discovery to the URL the server names, instead of the well-known guess.
// ---------------------------------------------------------------------------

/// HTTP mock for the §24c wiring test: the Protected Resource Metadata is
/// reachable ONLY at the challenge-named URL (the well-known guess 404s), and
/// the discovered issuer's AS metadata/token endpoint use a HOST distinct from
/// any well-known-guess fallback host, so a passing test proves the CHALLENGE
/// URL was actually used end to end (not merely that discovery succeeded via
/// some other path).
struct ChallengeAwareAs {
    requests: Mutex<Vec<HttpRequest>>,
    refresh_body: String,
}
impl ChallengeAwareAs {
    fn new(refresh_body: &str) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            refresh_body: refresh_body.into(),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait]
impl HttpTransport for ChallengeAwareAs {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(req.clone());
        let (status, body) = if req.url == "https://mcp.example.com/custom-prm-location" {
            (
                200,
                r#"{"resource":"https://mcp.example.com/","authorization_servers":["https://as-from-challenge.example.com"]}"#
                    .to_string(),
            )
        } else if req.url.contains("oauth-protected-resource") {
            // The well-known guess must never be reached once the challenge
            // names a URL — 404 it so a wrongly-used guess fails loudly
            // instead of silently degrading to some other working path.
            (404, String::new())
        } else if req.url
            == "https://as-from-challenge.example.com/.well-known/oauth-authorization-server"
        {
            (
                200,
                r#"{"authorization_endpoint":"https://as-from-challenge.example.com/authorize","token_endpoint":"https://as-from-challenge.example.com/token"}"#.to_string(),
            )
        } else if req.url == "https://as-from-challenge.example.com/token" {
            (200, self.refresh_body.clone())
        } else {
            (404, String::new())
        };
        Ok(HttpResponse {
            status,
            headers: vec![],
            body,
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}

#[tokio::test]
async fn connect_401_challenge_resource_metadata_url_drives_prm_fetch() {
    let refresh = r#"{"access_token":"access-challenge","refresh_token":"refresh-challenge","expires_in":3600}"#;
    let mock_as = ChallengeAwareAs::new(refresh);
    let challenge =
        r#"Bearer resource_metadata="https://mcp.example.com/custom-prm-location""#.to_string();
    // First connect attempt 401s carrying the challenge; the retry succeeds.
    let transport = RecordingTransport::new(1).with_401_challenge(&challenge);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let config = http_cfg("challenge-srv", Some(oauth_block(Some("preset-client"))));
    let key = oauth::server_key("challenge-srv", &config.spec);

    // Seed an UNEXPIRED token (so the first connect uses it and 401s) + a
    // refresh token, so the retry goes through `reauth_oauth_spec`'s discovery
    // call rather than a fresh interactive flow.
    let stored = oauth::StoredTokens {
        access_token: "access-old".into(),
        refresh_token: Some("refresh-1".into()),
        expires_at_unix: 99_999,
        client_id: Some("dcr-issued-7".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    let data = SecureStorageData::new(
        bytes,
        SecureStorageMetadata {
            created_at: SystemTime::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("mcp_oauth_tokens".into()),
        },
    );
    storage
        .store(oauth::MCP_OAUTH_SERVICE, &key, data)
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry
        .connect(config)
        .await
        .expect("connect ok after challenge-driven reauth");

    assert_eq!(transport.connect_count(), 2);
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer access-challenge")
    );
    let requested: Vec<String> = mock_as.requests().into_iter().map(|r| r.url).collect();
    assert!(
        requested.contains(&"https://mcp.example.com/custom-prm-location".to_string()),
        "the challenge-named resource_metadata URL must be fetched; got {requested:?}"
    );
    assert!(
        !requested
            .iter()
            .any(|u| u.contains("/.well-known/oauth-protected-resource/v1")),
        "the well-known guess must NOT be tried once the challenge names a URL; got {requested:?}"
    );
    assert!(
        requested.contains(&"https://as-from-challenge.example.com/token".to_string()),
        "token exchange must hit the AS discovered via the challenge's issuer; got {requested:?}"
    );
}

// ---------------------------------------------------------------------------
// §19: static/helper-minted Authorization is authoritative over OAuth.
// ---------------------------------------------------------------------------

/// Seed a valid (unexpired) OAuth token so that, if the §19 gate is broken,
/// `resolve_oauth_spec` takes its synchronous "stored, unexpired" branch
/// (no interactive flow, no network) — the bug is then observable purely as
/// a WRONG header value, never as a hang.
async fn seed_unexpired_oauth_token(storage: &Arc<MemStorage>, key: &str) {
    let stored = oauth::StoredTokens {
        access_token: "oauth-bearer-should-not-win".into(),
        refresh_token: Some("refresh-x".into()),
        expires_at_unix: 99_999,
        client_id: Some("client-x".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    let data = SecureStorageData::new(
        bytes,
        SecureStorageMetadata {
            created_at: SystemTime::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("mcp_oauth_tokens".into()),
        },
    );
    storage
        .store(oauth::MCP_OAUTH_SERVICE, key, data)
        .await
        .unwrap();
}

/// Gap 1: a static `headers.Authorization` on a server that ALSO has an
/// `oauth` block must never be overwritten by the OAuth Bearer, even when a
/// valid stored token is available.
#[tokio::test]
async fn static_authorization_header_survives_oauth_bearer() {
    let mock_as = MockAs::new("{}", "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let mut headers = lingxi_core::host::McpHeaders::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer configured-static".into(),
    );
    let config = McpServerConfig {
        name: "static-with-oauth".into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers,
            headers_helper: None,
            oauth: Some(oauth_block(Some("client-x"))),
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    };
    let key = oauth::server_key("static-with-oauth", &config.spec);
    seed_unexpired_oauth_token(&storage, &key).await;

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry.connect(config).await.expect("connect ok");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer configured-static"),
        "OAuth must never overwrite a static Authorization header"
    );
    assert!(
        mock_as.requests().is_empty(),
        "OAuth must never even be attempted once a static Authorization header is set"
    );
}

/// Gap 1 (classification): a static `headers.Authorization` server (no
/// `oauth` block) that is rejected with a 401 must surface the oracle's exact
/// `AUTH_HEADER_REJECTED` copy, not the raw transport error — and must not be
/// retried (no OAuth, no headersHelper to refresh).
#[tokio::test]
async fn static_authorization_header_rejection_is_classified() {
    let transport = RecordingTransport::new(usize::MAX);
    let mut headers = lingxi_core::host::McpHeaders::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer configured-static".into(),
    );
    let config = McpServerConfig {
        name: "static-rejected".into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers,
            headers_helper: None,
            oauth: None,
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    };

    let registry = McpRegistry::new(transport.clone() as Arc<dyn McpTransport>);
    let err = registry
        .connect(config)
        .await
        .expect_err("must be rejected");

    assert_eq!(
        err.to_string(),
        "connection failed: Server rejected the configured Authorization header (HTTP 401). \
Check that the token is valid for this MCP endpoint — OAuth fallback is disabled when \
headers.Authorization is set."
    );
    assert_eq!(transport.connect_count(), 1, "no retry for a static header");
}

/// Gap 2/3: a `headersHelper` that mints `Authorization` on a server that
/// ALSO has an `oauth` block must never be overwritten by the OAuth Bearer.
#[cfg(unix)]
#[tokio::test]
async fn headers_helper_minted_authorization_survives_oauth_bearer() {
    let mock_as = MockAs::new("{}", "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let config = McpServerConfig {
        name: "helper-with-oauth".into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: Some(r#"printf '{"Authorization":"Bearer helper-minted"}'"#.into()),
            oauth: Some(oauth_block(Some("client-x"))),
        },
        // User scope so the headersHelper trust-dialog check is skipped.
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    };
    // `oauth::server_key` hashes `{type, url, headers}`, so a stored-token
    // lookup keys on the spec's headers too. Seed under the key the
    // POST-helper-merge spec would hash to (the raw config's `headers` is
    // empty; the helper mints `Authorization` into it) — the same key
    // `resolve_oauth_spec` would look up if it (wrongly) ran here at all.
    // This is what keeps this test deterministic instead of hanging on a
    // never-driven interactive OAuth flow when the §19 gate is missing.
    let post_helper_spec = McpTransportSpec::Http {
        url: "https://mcp.example.com/v1".into(),
        headers: lingxi_core::host::McpHeaders::from_iter([(
            "Authorization".to_string(),
            "Bearer helper-minted".to_string(),
        )]),
        headers_helper: Some(r#"printf '{"Authorization":"Bearer helper-minted"}'"#.into()),
        oauth: Some(oauth_block(Some("client-x"))),
    };
    let key = oauth::server_key("helper-with-oauth", &post_helper_spec);
    seed_unexpired_oauth_token(&storage, &key).await;

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry.connect(config).await.expect("connect ok");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer helper-minted"),
        "OAuth must never overwrite a headersHelper-minted Authorization header"
    );
    assert!(
        mock_as.requests().is_empty(),
        "OAuth must never even be attempted once the helper mints Authorization"
    );
}

/// Gap 2 (classification): a `headersHelper` that mints `Authorization` (no
/// `oauth` block) rejected with a 401 must surface the oracle's exact
/// `HEADERS_HELPER_AUTH_REJECTED` copy — even after the existing
/// rerun-helper-once-on-auth-failure retry (proving the retried failure is
/// ALSO reclassified, not just the first one).
#[cfg(unix)]
#[tokio::test]
async fn headers_helper_minted_authorization_rejection_is_classified() {
    let transport = RecordingTransport::new(usize::MAX);
    let config = McpServerConfig {
        name: "helper-rejected".into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: Some(r#"printf '{"Authorization":"Bearer helper-minted"}'"#.into()),
            oauth: None,
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    };

    let registry = McpRegistry::new(transport.clone() as Arc<dyn McpTransport>);
    let err = registry
        .connect(config)
        .await
        .expect_err("must be rejected");

    assert_eq!(
        err.to_string(),
        "connection failed: Server rejected the Authorization header minted by the configured \
headersHelper (HTTP 401). Check that the helper command returns a valid credential for this \
MCP endpoint — OAuth fallback is disabled when the helper supplies Authorization."
    );
    // Initial attempt + the pre-existing "helper may emit short-lived
    // credentials, retry once" rerun — both fail, and the SECOND failure is
    // the one classified above.
    assert_eq!(transport.connect_count(), 2);
}

/// Gap 2 (predicate): a configured `headersHelper` that mints some OTHER
/// header (not `Authorization`) must NOT suppress OAuth — `helperMintsAuthHeader`
/// is "did the helper's output carry this exact key", not "is a helper
/// configured at all". OAuth's Bearer must still be injected and must
/// coexist with the helper's own header.
#[cfg(unix)]
#[tokio::test]
async fn headers_helper_minting_other_header_does_not_suppress_oauth() {
    let mock_as = MockAs::new("{}", "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let config = McpServerConfig {
        name: "helper-other-header".into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com/v1".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: Some(r#"printf '{"X-Api-Key":"helper-key"}'"#.into()),
            oauth: Some(oauth_block(Some("client-x"))),
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    };
    // The helper mints only `X-Api-Key`, so the post-helper spec's headers
    // are `{X-Api-Key: "helper-key"}` when `resolve_oauth_spec` hashes it.
    let post_helper_spec = McpTransportSpec::Http {
        url: "https://mcp.example.com/v1".into(),
        headers: lingxi_core::host::McpHeaders::from_iter([(
            "X-Api-Key".to_string(),
            "helper-key".to_string(),
        )]),
        headers_helper: Some(r#"printf '{"X-Api-Key":"helper-key"}'"#.into()),
        oauth: Some(oauth_block(Some("client-x"))),
    };
    let key = oauth::server_key("helper-other-header", &post_helper_spec);
    seed_unexpired_oauth_token(&storage, &key).await;

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry.connect(config).await.expect("connect ok");

    let seen = transport.last_spec();
    assert_eq!(
        spec_auth_header(&seen).as_deref(),
        Some("Bearer oauth-bearer-should-not-win"),
        "OAuth must still inject its Bearer when the helper mints a DIFFERENT header"
    );
    if let McpTransportSpec::Http { headers, .. } = seen {
        assert_eq!(
            headers.get("X-Api-Key").map(String::as_str),
            Some("helper-key")
        );
    } else {
        panic!("expected Http spec");
    }
}

// ---------------------------------------------------------------------------
// RESIDUAL 3 (A): token revocation (RFC 7009) on disconnect.
// ---------------------------------------------------------------------------

/// Seed a valid token, connect, then disconnect: the registry POSTs a revoke
/// for the refresh token (first) and the access token (second) to the AS
/// `revocation_endpoint`, then clears the local blob.
#[tokio::test]
async fn disconnect_revokes_tokens_and_clears_local() {
    let mock_as = MockAs::new("{}", "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let config = http_cfg("revoker", Some(oauth_block(Some("preset-client"))));
    let key = oauth::server_key("revoker", &config.spec);

    // Seed an UNEXPIRED token so connect uses it directly (no flow / refresh).
    let stored = oauth::StoredTokens {
        access_token: "access-live".into(),
        refresh_token: Some("refresh-live".into()),
        expires_at_unix: 99_999,
        client_id: Some("dcr-issued-7".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    let data = SecureStorageData::new(
        bytes,
        SecureStorageMetadata {
            created_at: SystemTime::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("mcp_oauth_tokens".into()),
        },
    );
    storage
        .store(oauth::MCP_OAUTH_SERVICE, &key, data)
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry.connect(config.clone()).await.expect("connect ok");
    registry.disconnect("revoker").await.expect("disconnect ok");

    // Two revoke POSTs hit the revocation_endpoint: refresh first, then access.
    let revokes: Vec<_> = mock_as
        .requests()
        .into_iter()
        .filter(|r| r.url.contains("/revoke"))
        .collect();
    assert_eq!(revokes.len(), 2, "one revoke per token");
    let b0 = revokes[0].body.as_deref().unwrap();
    let b1 = revokes[1].body.as_deref().unwrap();
    assert!(
        b0.contains("token=refresh-live") && b0.contains("token_type_hint=refresh_token"),
        "refresh token revoked first; body={b0}"
    );
    assert!(
        b1.contains("token=access-live") && b1.contains("token_type_hint=access_token"),
        "access token revoked second; body={b1}"
    );
    // Public client → client_id in the body (no Basic header).
    assert!(b0.contains("client_id=dcr-issued-7"));

    // Local blob is cleared regardless of server result.
    let reloaded = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap();
    assert!(reloaded.is_none(), "local tokens cleared after revocation");
}

/// When the AS advertises no `revocation_endpoint`, no revoke POST is sent but
/// the local tokens are still cleared.
#[tokio::test]
async fn disconnect_without_revocation_endpoint_still_clears() {
    // MockAsNoRevoke: same as MockAs but metadata omits revocation_endpoint.
    struct NoRevoke {
        requests: Mutex<Vec<HttpRequest>>,
    }
    #[async_trait]
    impl HttpTransport for NoRevoke {
        async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            let url = req.url.clone();
            self.requests.lock().unwrap().push(req);
            let (status, body) = if url.contains("oauth-protected-resource") {
                (404, String::new())
            } else if url.contains("oauth-authorization-server") {
                (
                    200,
                    r#"{"authorization_endpoint":"https://as.example.com/authorize","token_endpoint":"https://as.example.com/token"}"#
                        .to_string(),
                )
            } else {
                (404, String::new())
            };
            Ok(HttpResponse {
                status,
                headers: vec![],
                body,
                body_bytes: Vec::new(),
            })
        }
        async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
            Err(HttpError::InvalidRequest("unused".into()))
        }
    }

    let http = Arc::new(NoRevoke {
        requests: Mutex::new(Vec::new()),
    });
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let config = http_cfg("norev", Some(oauth_block(Some("preset-client"))));
    let key = oauth::server_key("norev", &config.spec);
    let stored = oauth::StoredTokens {
        access_token: "access-live".into(),
        refresh_token: Some("refresh-live".into()),
        expires_at_unix: 99_999,
        client_id: Some("c".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: http.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    registry.connect(config).await.expect("connect ok");
    registry.disconnect("norev").await.expect("disconnect ok");

    assert!(
        !http
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.url.contains("/revoke")),
        "no revoke POST when endpoint absent"
    );
    let reloaded = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap();
    assert!(
        reloaded.is_none(),
        "tokens cleared even without revocation endpoint"
    );
}

// ---------------------------------------------------------------------------
// RESIDUAL 3 (B): step-up scope (403 insufficient_scope) → re-auth + retry.
// ---------------------------------------------------------------------------

/// A seeded valid token connects and 403s with `insufficient_scope` requiring
/// `read:elevated`. The registry runs a fresh interactive flow whose authorize
/// URL carries the elevated scope, persists `step_up_scope`, then retries and
/// attaches the freshly minted Bearer. The cache is cleared on the new grant.
#[tokio::test]
async fn connect_403_insufficient_scope_triggers_step_up_reauth() {
    let exchange = r#"{"access_token":"elevated-access","refresh_token":"r","expires_in":3600}"#;
    let mock_as = MockAs::new(exchange, "{}");
    // First connect 403s requiring read:elevated; the post-reauth connect succeeds.
    let transport = RecordingTransport::with_403_first("read:elevated");
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, mut url_rx) = url_capture();

    // client_id configured → no DCR; the authorize URL is the step-up signal.
    let config = http_cfg("stepup", Some(oauth_block(Some("preset-client"))));
    let key = oauth::server_key("stepup", &config.spec);

    // Seed an UNEXPIRED token so the first connect uses it and 403s.
    let stored = oauth::StoredTokens {
        access_token: "narrow-access".into(),
        refresh_token: Some("narrow-refresh".into()),
        expires_at_unix: 99_999,
        client_id: Some("preset-client".into()),
        client_secret: None,
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    // Drive the fake browser once the (step-up) authorize URL is surfaced, and
    // capture that URL so we can assert it carries the elevated scope.
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured2 = captured.clone();
    let browser = tokio::spawn(async move {
        let url = url_rx.recv().await.expect("auth url surfaced");
        *captured2.lock().unwrap() = Some(url.clone());
        drive_browser(&url).await;
    });

    let registry2 = Arc::new(registry);
    registry2
        .connect(config.clone())
        .await
        .expect("connect ok after step-up");
    browser.await.unwrap();

    // Two connect attempts: narrow token (403), then step-up token.
    assert_eq!(transport.connect_count(), 2);
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer elevated-access")
    );

    // The step-up authorize URL requested the elevated scope (urlencoded).
    let auth_url = captured
        .lock()
        .unwrap()
        .clone()
        .expect("authorize url captured");
    assert!(
        auth_url.contains("scope=read%3Aelevated"),
        "step-up authorize URL must request the elevated scope; url={auth_url}"
    );

    // The new grant cleared the cached step-up scope (fresh tokens carry it).
    let reloaded = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.access_token, "elevated-access");
    assert_eq!(
        reloaded.step_up_scope, None,
        "step_up_scope cleared on fresh grant"
    );
}

/// Unit: the 403 detector extracts a quoted scope, an unquoted scope, ignores
/// non-403 / non-insufficient_scope errors.
#[test]
fn step_up_scope_extraction() {
    use mcp::registry::test_support::error_is_403_insufficient_scope as detect;
    assert_eq!(
        detect(&McpError::Connection(
            "HTTP 403: error=\"insufficient_scope\", scope=\"a b\"".into()
        )),
        Some("a b".to_string())
    );
    assert_eq!(
        detect(&McpError::Handshake(
            "403 insufficient_scope scope=read:x, realm=foo".into()
        )),
        Some("read:x".to_string())
    );
    // 403 without insufficient_scope → not step-up.
    assert_eq!(
        detect(&McpError::Connection("HTTP 403 Forbidden".into())),
        None
    );
    // 401 → not step-up.
    assert_eq!(
        detect(&McpError::Connection(
            "HTTP 401 insufficient_scope scope=x".into()
        )),
        None
    );
}

/// Unit: the 403 detector's PRIMARY match is the structural
/// `McpError::HttpResponse` carrier (SSE's pre-flight GET returns it
/// directly; Streamable HTTP's `initialize` unwraps it from the synthetic
/// JSON-RPC error's `data` field via `handshake_error`,
/// `platforms/posix/src/mcp.rs`) — no string flattening or substring
/// matching involved. The prior test covers the string-fallback arms; this
/// covers the structured arms the fallback exists alongside.
#[test]
fn step_up_scope_extraction_from_structured_http_response() {
    use mcp::registry::test_support::error_is_403_insufficient_scope as detect;
    assert_eq!(
        detect(&McpError::HttpResponse {
            status: 403,
            www_authenticate: Some(
                "Bearer error=\"insufficient_scope\", scope=\"mcp:elevated\"".into()
            ),
        }),
        Some("mcp:elevated".to_string())
    );
    // 403 without insufficient_scope → not step-up, even structurally.
    assert_eq!(
        detect(&McpError::HttpResponse {
            status: 403,
            www_authenticate: Some("Bearer realm=\"mcp\"".into()),
        }),
        None
    );
    // 401 structurally → not step-up (that's `error_is_401`'s job).
    assert_eq!(
        detect(&McpError::HttpResponse {
            status: 401,
            www_authenticate: Some("Bearer error=\"insufficient_scope\", scope=\"x\"".into()),
        }),
        None
    );
}

// ---------------------------------------------------------------------------
// RESIDUAL 3 (C): XAA gate + registry-driven cross-app-access.
// ---------------------------------------------------------------------------

use mcp::registry::{XaaConfigProvider, XaaInputs};

/// Serializes the two XAA tests that mutate the process-global
/// `LINGXI_ENABLE_XAA` env var so they can't race under the parallel runner.
static XAA_ENV_LOCK: Mutex<()> = Mutex::new(());

/// An `oauth.xaa=Some(true)` server with `LINGXI_ENABLE_XAA` unset hard-fails
/// with actionable copy (auth.ts:871-876) instead of degrading to consent.
//
// The serialization guard is intentionally held across the `connect().await`
// (the env must stay set for the whole call). These tests run on the default
// current-thread runtime, so this is safe; allow the lint.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_flagged_server_without_enable_flag_hard_fails() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Ensure the gate env is unset for this test.
    std::env::remove_var("LINGXI_ENABLE_XAA");

    let mock_as = MockAs::new("{}", "{}");
    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-srv", Some(oauth));

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: mock_as as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: None,
        });

    let err = registry
        .connect(config)
        .await
        .expect_err("xaa gate must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("LINGXI_ENABLE_XAA"),
        "actionable enable-flag copy; got: {msg}"
    );
    // The transport was never connected (the gate fires before connect).
    assert_eq!(transport.connect_count(), 0);
}

/// Mock transport for the live XAA registry test: answers PRM / AS-metadata /
/// `IdP` token-exchange / AS jwt-bearer.
struct XaaHttp;
#[async_trait]
impl HttpTransport for XaaHttp {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let url = &req.url;
        let (status, body) = if url.contains("oauth-protected-resource") {
            (200, r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com"]}"#.to_string())
        } else if url.contains("oauth-authorization-server") {
            (200, r#"{"issuer":"https://as.example.com","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string())
        } else if url.contains("idp.example.com/token") {
            (200, r#"{"access_token":"id-jag","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag"}"#.to_string())
        } else if url.contains("as.example.com/token") {
            (
                200,
                r#"{"access_token":"xaa-access","token_type":"Bearer","expires_in":3600}"#
                    .to_string(),
            )
        } else {
            (404, String::new())
        };
        Ok(HttpResponse {
            status,
            headers: vec![],
            body,
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}

/// Mock XAA inputs provider for the live registry test.
struct XaaTestProvider;
#[async_trait]
impl XaaConfigProvider for XaaTestProvider {
    async fn xaa_inputs(
        &self,
        _server_name: &str,
        _server_url: &str,
    ) -> Result<Option<XaaInputs>, McpError> {
        Ok(Some(XaaInputs {
            client_id: "as-client".into(),
            client_secret: "as-secret".into(),
            idp_client_id: "idp-client".into(),
            idp_client_secret: None,
            idp_id_token: "the-id-token".into(),
            idp_token_endpoint: "https://idp.example.com/token".into(),
        }))
    }
}

/// With the flag set + a wired provider, an XAA server resolves its Bearer via
/// the cross-app-access exchange and attaches it to the spec; the persisted
/// blob carries the AS confidential `client_id` + `client_secret`.
#[allow(clippy::await_holding_lock)] // see the sibling test's note
#[tokio::test]
async fn xaa_enabled_drives_exchange_and_attaches_bearer() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-live", Some(oauth));
    let key = oauth::server_key("xaa-live", &config.spec);

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: Arc::new(XaaHttp) as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    registry.connect(config).await.expect("xaa connect ok");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer xaa-access")
    );
    // Persisted blob carries the AS confidential client_id + client_secret so
    // RFC-7009 revocation can authenticate the confidential client.
    let stored = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap()
        .expect("xaa tokens persisted");
    assert_eq!(stored.access_token, "xaa-access");
    assert_eq!(stored.client_id.as_deref(), Some("as-client"));
    assert_eq!(stored.client_secret.as_deref(), Some("as-secret"));
}

// ---------------------------------------------------------------------------
// §26b: XAA refresh — no proactive window absent a refresh token (delta 1),
// a stored refresh token takes the ordinary refresh route (delta 4), and a
// 401 re-auth stays on the XAA path instead of falling to interactive
// consent (delta 3). Single-flight (delta 2) is crate-private and covered by
// `resolve_xaa_token_inner`'s in-crate unit test in `mcp/src/registry.rs`.
// ---------------------------------------------------------------------------

/// Panics if ever asked to make a request — proves a code path took NO
/// network action at all (the "reuse the cache, don't touch the wire" case).
struct PanicOnRequest;
#[async_trait]
impl HttpTransport for PanicOnRequest {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        panic!(
            "XAA must reuse the cached access token without any network call; \
             got a request to {}",
            req.url
        );
    }
    async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}

/// §26b delta 1 (regression guard): a cached XAA access token far from expiry
/// (well outside the 300s window) and with no refresh token is reused
/// outright — no silent exchange, no network call at all.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_no_refresh_token_far_from_expiry_reuses_cache_without_exchange() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-farexp", Some(oauth));
    let key = oauth::server_key("xaa-farexp", &config.spec);

    let stored = oauth::StoredTokens {
        access_token: "cached-far".into(),
        refresh_token: None,
        expires_at_unix: 1_000 + 3_600, // 3600s away — well outside the 300s window
        client_id: Some("as-client".into()),
        client_secret: Some("as-secret".into()),
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: Arc::new(PanicOnRequest) as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    registry
        .connect(config)
        .await
        .expect("xaa connect ok, cache reused");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer cached-far")
    );
}

/// §26b delta 1: a cached XAA access token with NO refresh token that expires
/// within 300s drives the silent exchange even though it is not yet
/// literally expired — the oracle's proactive window. Before the fix, the
/// port only re-exchanged on ACTUAL expiry, so this asserts the FRESH
/// exchanged token, not the stale-but-not-yet-expired cached one.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_no_refresh_token_near_expiry_triggers_silent_exchange() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-nearexp", Some(oauth));
    let key = oauth::server_key("xaa-nearexp", &config.spec);

    let stored = oauth::StoredTokens {
        access_token: "cached-near".into(),
        refresh_token: None,
        expires_at_unix: 1_000 + 100, // 100s away — inside the 300s window
        client_id: Some("as-client".into()),
        client_secret: Some("as-secret".into()),
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: Arc::new(XaaHttp) as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    registry
        .connect(config)
        .await
        .expect("xaa connect ok via silent exchange");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer xaa-access"),
        "must be the FRESH exchanged token, not the stale (soon-to-expire) cached one"
    );
}

/// HTTP mock for §26b delta 4: answers PRM + AS-metadata discovery and an
/// ordinary `refresh_token` grant authenticated via `client_secret_basic`;
/// panics if the `IdP` token-exchange leg or a `jwt-bearer` grant is ever hit —
/// proving a stored refresh token takes the ordinary refresh route instead of
/// re-running the full IdP+AS exchange chain.
struct XaaRefreshOnlyAs {
    requests: Mutex<Vec<HttpRequest>>,
}
impl XaaRefreshOnlyAs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait]
impl HttpTransport for XaaRefreshOnlyAs {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(req.clone());
        let url = &req.url;
        let body = req.body.as_deref().unwrap_or("");
        assert!(
            !url.contains("idp.example.com"),
            "delta 4: a stored refresh token must never drive the IdP token-exchange leg"
        );
        if url.contains("oauth-protected-resource") {
            return Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com"]}"#.into(),
                body_bytes: Vec::new(),
            });
        }
        if url.contains("oauth-authorization-server") {
            return Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"issuer":"https://as.example.com","authorization_endpoint":"https://as.example.com/authorize","token_endpoint":"https://as.example.com/token"}"#.into(),
                body_bytes: Vec::new(),
            });
        }
        if url == "https://as.example.com/token" {
            assert!(
                body.contains("grant_type=refresh_token"),
                "must be an ordinary refresh grant; body={body}"
            );
            assert!(
                !body.contains("jwt-bearer") && !body.contains("assertion="),
                "must NOT be the XAA jwt-bearer grant; body={body}"
            );
            let auth = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.clone());
            assert!(
                auth.as_deref().is_some_and(|v| v.starts_with("Basic ")),
                "confidential client must authenticate via client_secret_basic; got {auth:?}"
            );
            return Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"access_token":"xaa-refreshed","refresh_token":"xaa-refresh-2","token_type":"Bearer","expires_in":3600}"#.into(),
                body_bytes: Vec::new(),
            });
        }
        Ok(HttpResponse {
            status: 404,
            headers: vec![],
            body: String::new(),
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}

/// §26b delta 4: a stored, expired XAA token WITH a refresh token drives the
/// ordinary refresh grant (confidential `client_secret_basic`) rather than
/// re-running the full IdP+AS exchange chain.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_stored_refresh_token_uses_ordinary_refresh_not_full_exchange() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();
    let http = XaaRefreshOnlyAs::new();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-refresh", Some(oauth));
    let key = oauth::server_key("xaa-refresh", &config.spec);

    let stored = oauth::StoredTokens {
        access_token: "xaa-stale".into(),
        refresh_token: Some("xaa-refresh-1".into()),
        expires_at_unix: 500, // already expired (clock = 1000)
        client_id: Some("as-client".into()),
        client_secret: Some("as-secret".into()),
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: http.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage.clone() as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    registry
        .connect(config)
        .await
        .expect("xaa connect ok via ordinary refresh");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer xaa-refreshed")
    );
    assert!(
        http.requests()
            .iter()
            .any(|r| r.url == "https://as.example.com/token"),
        "the ordinary refresh leg was hit"
    );
    let restored = oauth::load_tokens(&(storage as Arc<dyn SecureStorage>), &key)
        .await
        .unwrap()
        .expect("refreshed tokens persisted");
    assert_eq!(restored.access_token, "xaa-refreshed");
    assert_eq!(restored.refresh_token.as_deref(), Some("xaa-refresh-2"));
    assert_eq!(
        restored.client_secret.as_deref(),
        Some("as-secret"),
        "the confidential client_secret must survive the refresh round-trip"
    );
}

/// §26b delta 3: a server 401 for an XAA-flagged server must stay on the XAA
/// path — never fall through to `run_interactive_oauth` (a 300s wait for a
/// browser callback that will never arrive in this test). The registry's own
/// module comment already promises this ("XAA is the ONLY auth path"); this
/// asserts it holds across a 401 specifically. A short `tokio::time::timeout`
/// races the real 300s callback wait so a regression fails in ~2s instead of
/// hanging for 5 minutes.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_401_reauth_never_falls_to_interactive_consent() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    // First connect attempt 401s (the cached XAA token was rejected by the
    // server even though the registry still considers it locally unexpired
    // and far from the 300s window); the retry (post re-auth) succeeds.
    let transport = RecordingTransport::new(1);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, mut url_rx) = url_capture();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-401", Some(oauth));
    let key = oauth::server_key("xaa-401", &config.spec);

    // No refresh token stored — the shape §26b delta 3 targets: the old
    // `reauth_oauth_spec` had no xaa arm, so a refresh-token-less XAA server
    // fell into `None => run_interactive_oauth(...)`.
    let stored = oauth::StoredTokens {
        access_token: "xaa-access-old".into(),
        refresh_token: None,
        expires_at_unix: 1_000 + 3_600,
        client_id: Some("as-client".into()),
        client_secret: Some("as-secret".into()),
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: Arc::new(XaaHttp) as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    let outcome = tokio::time::timeout(Duration::from_secs(2), registry.connect(config)).await;
    std::env::remove_var("LINGXI_ENABLE_XAA");

    let connect_result = outcome.expect(
        "connect() must not hang waiting for interactive consent — an XAA 401 must stay \
         on the silent exchange/refresh path (§26b delta 3), never run_interactive_oauth",
    );
    connect_result.expect("xaa connect ok after 401 reauth");

    assert_eq!(
        transport.connect_count(),
        2,
        "stale bearer 401s, freshly-exchanged bearer retries"
    );
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer xaa-access")
    );
    assert!(
        url_rx.try_recv().is_err(),
        "no interactive authorization URL should ever be surfaced for an XAA server"
    );
}

/// §26b delta 3, second arm: a 403 `insufficient_scope` must stay on the XAA
/// path exactly as the 401 does. `connect_locked_inner` evaluates the 403
/// branch BEFORE the 401 branch, so a `step_up_oauth_spec` without an xaa
/// guard WINS the race and drives `run_interactive_oauth` — binding a
/// loopback listener and opening a browser on a deployment that has no
/// interactive consent surface, contradicting `resolve_oauth_spec`'s own
/// "XAA is the ONLY auth path" comment. Oracle `pEr` (@182200767) opens with
/// `if(t.oauth?.xaa){...await gt(...);return}`, so no step-up can ever reach
/// `redirectToAuthorization`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_403_step_up_never_falls_to_interactive_consent() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    // First connect 403s with `insufficient_scope`; the retry succeeds.
    let transport = RecordingTransport::with_403_first("mcp:elevated");
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, mut url_rx) = url_capture();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-403", Some(oauth));
    let key = oauth::server_key("xaa-403", &config.spec);

    let stored = oauth::StoredTokens {
        access_token: "xaa-access-old".into(),
        refresh_token: None,
        expires_at_unix: 1_000 + 3_600,
        client_id: Some("as-client".into()),
        client_secret: Some("as-secret".into()),
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: Arc::new(XaaHttp) as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    let outcome = tokio::time::timeout(Duration::from_secs(2), registry.connect(config)).await;
    std::env::remove_var("LINGXI_ENABLE_XAA");

    let connect_result = outcome.expect(
        "connect() must not hang waiting for interactive consent — an XAA 403          insufficient_scope must stay on the silent exchange path, never run_interactive_oauth",
    );
    connect_result.expect("xaa connect ok after 403 step-up");

    assert_eq!(
        transport.connect_count(),
        2,
        "403 once, freshly-exchanged bearer retries"
    );
    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer xaa-access")
    );
    assert!(
        url_rx.try_recv().is_err(),
        "no interactive authorization URL should ever be surfaced for an XAA server"
    );
}

/// §26b delta 1, refresh-token arm: oracle `tokens()` (@182213696) applies its
/// 300s proactive window to the case where a refresh token IS stored —
/// `if(r!=null&&r<=300&&n.refreshToken&&!d){...refreshAuthorization(...)}`
/// runs after the XAA block for every server. Gating reuse on hard expiry
/// instead hands the transport a token 60 seconds from death.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn xaa_stored_refresh_token_inside_300s_window_refreshes_proactively() {
    let _guard = XAA_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("LINGXI_ENABLE_XAA", "1");

    let transport = RecordingTransport::new(0);
    let storage = MemStorage::new();
    let clock = TestClock::new(1_000);
    let (on_url, _rx) = url_capture();
    let http = XaaRefreshOnlyAs::new();

    let oauth = McpOAuthConfigDto {
        client_id: Some("as-client".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: Some(true),
    };
    let config = http_cfg("xaa-proactive", Some(oauth));
    let key = oauth::server_key("xaa-proactive", &config.spec);

    let stored = oauth::StoredTokens {
        access_token: "xaa-nearly-dead".into(),
        refresh_token: Some("xaa-refresh-1".into()),
        // Still VALID (clock = 1000) but only 60s of life left — inside the
        // 300s window, so the oracle refreshes rather than serving it.
        expires_at_unix: 1_000 + 60,
        client_id: Some("as-client".into()),
        client_secret: Some("as-secret".into()),
        step_up_scope: None,
    };
    let bytes = serde_json::to_vec(&stored).unwrap();
    storage
        .store(
            oauth::MCP_OAUTH_SERVICE,
            &key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_tokens".into()),
                },
            ),
        )
        .await
        .unwrap();

    let registry =
        McpRegistry::new(transport.clone() as Arc<dyn McpTransport>).with_oauth(OAuthDeps {
            http: http.clone() as Arc<dyn HttpTransport>,
            clock: clock as Arc<dyn Clock>,
            storage: storage as Arc<dyn SecureStorage>,
            on_authorization_url: on_url,
            xaa_config: Some(Arc::new(XaaTestProvider)),
        });

    registry.connect(config).await.expect("xaa connect ok");
    std::env::remove_var("LINGXI_ENABLE_XAA");

    assert_eq!(
        spec_auth_header(&transport.last_spec()).as_deref(),
        Some("Bearer xaa-refreshed"),
        "a token inside the 300s window must be refreshed BEFORE it is used"
    );
    assert!(
        http.requests()
            .iter()
            .any(|r| r.url == "https://as.example.com/token"),
        "the proactive refresh leg must actually be hit; got {:?}",
        http.requests()
            .iter()
            .map(|r| r.url.clone())
            .collect::<Vec<_>>()
    );
}
