//! Fixture values execute immutable, hash-pinned Claude Code 2.1.322 helpers.
//! Fake MCP OAuth data only; this module never reads a credential store.
use super::*;
use lingxi_core::types::HttpResponse;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/mcp_oauth_2_1_322.json")).unwrap()
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap()
}

fn optional<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str()
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect()
    })
}

fn metadata() -> AuthServerMetadata {
    AuthServerMetadata {
        authorization_endpoint: "https://as.example/authorize".into(),
        token_endpoint: "https://as.example/token".into(),
        registration_endpoint: Some("https://as.example/register".into()),
        scopes_supported: None,
        scope: None,
        default_scope: None,
        revocation_endpoint: Some("https://as.example/revoke".into()),
        revocation_endpoint_auth_methods_supported: None,
        token_endpoint_auth_methods_supported: Some(vec!["none".into()]),
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
}

struct CaptureHttp {
    requests: Mutex<Vec<HttpRequest>>,
    statuses: Mutex<VecDeque<u16>>,
}
impl CaptureHttp {
    fn new(statuses: &[u16]) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            statuses: Mutex::new(statuses.iter().copied().collect()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait::async_trait]
impl HttpTransport for CaptureHttp {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let body = if request.url.ends_with("/register") {
            let mut registration: Value = serde_json::from_str(request.body.as_deref().unwrap())
                .expect("DCR request carries valid client metadata");
            registration["client_id"] = json!("fake-dcr-client");
            registration.to_string()
        } else {
            r#"{"access_token":"fake-access","token_type":"Bearer","expires_in":60}"#.to_owned()
        };
        self.requests.lock().unwrap().push(request);
        Ok(HttpResponse {
            status: self.statuses.lock().unwrap().pop_front().unwrap_or(200),
            headers: Vec::new(),
            body,
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::Connection("unused in OAuth fixture".into()))
    }
}

fn normalized_headers(request: &HttpRequest) -> Value {
    serde_json::to_value(
        request
            .headers
            .iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap()
}

#[test]
fn native_322_pkce_state_challenge_and_discovery_bytes() {
    let fixture = fixture();
    for case in fixture["pkceCases"].as_array().unwrap() {
        let bytes: [u8; 43] = case["bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap() as u8)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let (verifier, challenge) = pkce_from_random_bytes(&bytes);
        assert_eq!(verifier, string(case, "code_verifier"));
        assert_eq!(challenge, string(case, "code_challenge"));
    }
    let state = generate_state_token();
    assert_eq!(URL_SAFE_NO_PAD.decode(state).unwrap().len(), 32);
    for case in fixture["challengeCases"].as_array().unwrap() {
        let actual = parse_www_authenticate_challenge(string(case, "header"));
        let expected = &case["expected"];
        assert_eq!(
            actual.resource_metadata_url.as_deref(),
            optional(expected, "resource_metadata_url")
        );
        assert_eq!(actual.scope.as_deref(), optional(expected, "scope"));
        assert_eq!(actual.error.as_deref(), optional(expected, "error"));
        assert_eq!(
            actual.error_description.as_deref(),
            optional(expected, "error_description")
        );
    }
    for case in fixture["discoveryUrlCases"].as_array().unwrap() {
        assert_eq!(
            well_known_url(string(case, "base"), "oauth-protected-resource"),
            string(case, "prm")
        );
        assert_eq!(
            json!(authorization_metadata_urls(string(case, "base")).unwrap()),
            case["authorization"]
        );
    }
}

#[test]
fn native_322_authorize_and_scope_policy_bytes() {
    let fixture = fixture();
    for case in fixture["authorizeCases"].as_array().unwrap() {
        assert_eq!(
            authorize_url_from_material(
                string(case, "endpoint"),
                string(case, "client_id"),
                string(case, "redirect_uri"),
                string(case, "scope"),
                optional(case, "resource"),
                string(case, "state"),
                string(case, "challenge")
            ),
            string(case, "expected")
        );
    }
    for case in fixture["scopeCases"].as_array().unwrap() {
        let mut meta = metadata();
        meta.authorization_endpoint =
            "https://as.example/authorize?prompt=login&prompt=select_account".into();
        meta.scopes_supported = strings(&case["as_scopes"]);
        let resource_scopes = strings(&case["resource_scopes"]);
        let scope = sdk_requested_scope(
            optional(case, "requested"),
            resource_scopes.as_deref(),
            optional(case, "client_scope"),
            &meta,
        );
        assert_eq!(scope.as_deref(), optional(case, "sdk_scope"), "{case}");
        let configured = if case["pending"].as_bool().unwrap_or(false) {
            None
        } else {
            optional(case, "configured_scope")
        };
        let url = authorize_url_from_material_with_scope(
            &meta.authorization_endpoint,
            "fake-client",
            "http://localhost:31337/callback",
            scope.as_deref().unwrap_or_default(),
            None,
            "expected-state",
            "fixed-challenge",
            Some((&meta, configured)),
        );
        assert_eq!(url, string(case, "expected"), "{case}");
    }
}

#[tokio::test]
async fn native_322_dcr_json_and_grant_form_bytes() {
    let fixture = fixture();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    for case in fixture["registrationCases"].as_array().unwrap() {
        let capture = CaptureHttp::new(&[201]);
        let http: Arc<dyn HttpTransport> = capture.clone();
        assert_eq!(
            register_client_with_scope_order(
                &http,
                string(case, "url"),
                string(case, "redirect_uri"),
                "fake-server",
                optional(case, "scope").or_else(|| optional(case, "initial_scope")),
                optional(case, "initial_scope").is_some(),
            )
            .await
            .unwrap(),
            "fake-dcr-client"
        );
        let request = &capture.requests()[0];
        assert_eq!(request.body.as_deref(), Some(string(case, "body")));
        assert_eq!(normalized_headers(request), case["headers"]);
    }
    for case in fixture["grantCases"].as_array().unwrap() {
        let capture = CaptureHttp::new(&[]);
        let http: Arc<dyn HttpTransport> = capture.clone();
        let meta = metadata();
        if string(case, "kind") == "code" {
            exchange_code_with_resource(
                &http,
                &clock,
                &meta,
                "fake +~!*client",
                "fake +~!*code",
                "fixed-verifier-43",
                "http://localhost:31337/callback",
                optional(case, "resource"),
            )
            .await
            .unwrap();
        } else {
            refresh_tokens_once(
                &http,
                &clock,
                &meta,
                "fake +~!*client",
                None,
                "fake +~!*refresh",
                optional(case, "resource"),
                false,
            )
            .await
            .unwrap();
        }
        let request = &capture.requests()[0];
        assert_eq!(request.url, string(case, "url"));
        assert_eq!(
            request.body.as_deref(),
            Some(string(case, "body")),
            "{case}"
        );
        assert_eq!(normalized_headers(request), case["headers"]);
    }
}

#[tokio::test]
async fn native_322_client_auth_and_revocation_bytes() {
    let fixture = fixture();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    for case in fixture["authCases"].as_array().unwrap() {
        let mut meta = metadata();
        meta.token_endpoint_auth_methods_supported = strings(&case["supported"]);
        let secret = optional(&case["client"], "client_secret");
        let method = select_token_auth_method(
            secret.is_some(),
            meta.token_endpoint_auth_methods_supported
                .as_deref()
                .unwrap(),
        );
        let method_name = match method {
            TokenAuthMethod::SecretBasic => "client_secret_basic",
            TokenAuthMethod::SecretPost => "client_secret_post",
            TokenAuthMethod::None => "none",
        };
        assert_eq!(method_name, string(case, "method"));
        let capture = CaptureHttp::new(&[]);
        let http: Arc<dyn HttpTransport> = capture.clone();
        let result = refresh_tokens_once(
            &http,
            &clock,
            &meta,
            string(&case["client"], "client_id"),
            secret,
            "fake +~!*refresh",
            None,
            false,
        )
        .await;
        assert_eq!(result.is_err(), case.get("error").is_some(), "{case}");
        if result.is_err() {
            assert!(capture.requests().is_empty());
            continue;
        }
        let request = &capture.requests()[0];
        assert_eq!(
            request.body.as_deref(),
            Some(string(case, "body")),
            "{case}"
        );
        let mut headers = normalized_headers(request);
        headers.as_object_mut().unwrap().remove("accept");
        headers.as_object_mut().unwrap().remove("content-type");
        assert_eq!(headers, case["headers"], "{case}");
    }
    for case in fixture["revocationCases"].as_array().unwrap() {
        let capture = CaptureHttp::new(if case["retry"].as_bool().unwrap() {
            &[401, 200]
        } else {
            &[200]
        });
        let http: Arc<dyn HttpTransport> = capture.clone();
        revoke_token(
            &http,
            "https://as.example/revoke",
            "fake +~!*token",
            TokenTypeHint::RefreshToken,
            Some("fake +~!*client"),
            optional(case, "client_secret"),
            Some("fake-access"),
            string(case, "auth_method"),
        )
        .await
        .unwrap();
        let requests = capture.requests();
        let expected = case["requests"].as_array().unwrap();
        assert_eq!(requests.len(), expected.len());
        for (request, expected) in requests.iter().zip(expected) {
            assert_eq!(request.url, string(expected, "url"));
            assert_eq!(request.body.as_deref(), Some(string(expected, "body")));
            assert_eq!(normalized_headers(request), expected["headers"]);
        }
    }
}

// A minimal HTTP/1.1 transport keeps the socket test independent of a
// platform client and accepts only disposable loopback fixture endpoints.
struct LoopbackHttp;

async fn read_http_message(stream: &mut tokio::net::TcpStream) -> (String, String) {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0u8; 1024];
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "fixture HTTP headers incomplete");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut chunk = [0u8; 1024];
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "fixture HTTP body incomplete");
        bytes.extend_from_slice(&chunk[..count]);
    }
    (
        headers,
        String::from_utf8(bytes[end..end + length].to_vec()).unwrap(),
    )
}

#[async_trait::async_trait]
impl HttpTransport for LoopbackHttp {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        use tokio::io::AsyncWriteExt;
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.scheme(), "http");
        assert!(is_oauth_loopback(&url));
        let port = url.port_or_known_default().unwrap();
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|error| HttpError::Connection(error.to_string()))?;
        let method = match request.method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            _ => panic!("unexpected OAuth fixture method"),
        };
        let path = format!(
            "{}{}",
            url.path(),
            url.query()
                .map(|query| format!("?{query}"))
                .unwrap_or_default()
        );
        let body = request.body.unwrap_or_default();
        let mut wire = format!("{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in request.headers {
            wire.push_str(&format!("{name}: {value}\r\n"));
        }
        wire.push_str("\r\n");
        wire.push_str(&body);
        stream
            .write_all(wire.as_bytes())
            .await
            .map_err(|error| HttpError::Connection(error.to_string()))?;
        let (head, body) = read_http_message(&mut stream).await;
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
            .collect();
        Ok(HttpResponse {
            status,
            headers,
            body_bytes: body.as_bytes().to_vec(),
            body,
        })
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::Connection("unused in OAuth fixture".into()))
    }
}

fn get_request(url: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: url.to_owned(),
        headers: Vec::new(),
        body: None,
        body_bytes: None,
        timeout: None,
    }
}

#[tokio::test]
async fn native_322_loopback_discovery_dcr_pkce_callback_refresh_and_revocation() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server_url = format!("{base}/mcp?aud=one%20two");
    let traffic = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    let auth_query = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
    let server_base = base.clone();
    let server_traffic = traffic.clone();
    let server_auth = auth_query.clone();
    let peer = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, body) = read_http_message(&mut socket).await;
            let target = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            server_traffic
                .lock()
                .unwrap()
                .push((target.clone(), headers.clone(), body.clone()));
            let url = url::Url::parse(&format!("{server_base}{target}")).unwrap();
            let form = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<BTreeMap<_, _>>();
            let (status, response_body, extra) = match url.path() {
                "/.well-known/oauth-protected-resource/mcp" | "/.well-known/oauth-authorization-server/issuer" | "/.well-known/openid-configuration/issuer" => (404, String::new(), String::new()),
                "/.well-known/oauth-protected-resource" => (200, json!({"resource":format!("{server_base}/mcp"),"authorization_servers":[format!("{server_base}/issuer")],"scopes_supported":["read","write"]}).to_string(), String::new()),
                "/issuer/.well-known/openid-configuration" => (200, json!({"issuer":format!("{server_base}/issuer"),"jwks_uri":format!("{server_base}/keys"),"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"],"authorization_endpoint":format!("{server_base}/authorize"),"token_endpoint":format!("{server_base}/token"),"registration_endpoint":format!("{server_base}/register"),"revocation_endpoint":format!("{server_base}/revoke"),"response_types_supported":["code"],"scopes_supported":["read","write","offline_access"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],"authorization_response_iss_parameter_supported":true}).to_string(), String::new()),
                "/register" => {
                    let mut request: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(request["scope"], "read write offline_access");
                    assert_eq!(request["application_type"], "native");
                    assert_eq!(request["client_name"], "LingXi (fake-server)");
                    request["client_id"] = json!("fake-dcr-client");
                    (201, request.to_string(), String::new())
                }
                "/authorize" => {
                    let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
                    assert_eq!(query["scope"], "read write offline_access");
                    assert_eq!(query["prompt"], "consent");
                    let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
                    callback.query_pairs_mut().append_pair("code", "fake +~!*code").append_pair("state", &query["state"]).append_pair("iss", &format!("{server_base}/issuer"));
                    *server_auth.lock().unwrap() = query;
                    (302, String::new(), format!("Location: {callback}\r\n"))
                }
                "/token" => {
                    assert_eq!(form["client_id"], "fake-dcr-client");
                    if form["grant_type"] == "authorization_code" {
                        assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())), server_auth.lock().unwrap()["code_challenge"]);
                        assert_eq!(form["code"], "fake +~!*code");
                        assert_eq!(form["resource"], format!("{server_base}/mcp"));
                    } else { assert_eq!(form["refresh_token"], "fake-refresh"); }
                    (200, r#"{"access_token":"fake-access","refresh_token":"fake-refresh","token_type":"Bearer","expires_in":60}"#.into(), String::new())
                }
                "/revoke" => {
                    if headers.lines().any(|line| line.eq_ignore_ascii_case("authorization: Bearer fake-access")) {
                        assert!(!form.contains_key("client_id"));
                        (200, String::new(), String::new())
                    } else { (401, String::new(), String::new()) }
                }
                path => panic!("unexpected fixture path {path}"),
            };
            let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{response_body}", response_body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (browser_tx, browser_rx) = tokio::sync::oneshot::channel();
    let browser_tx = Arc::new(Mutex::new(Some(browser_tx)));
    let on_url: OnAuthorizationUrl = Arc::new(move |url| {
        let url = url.to_owned();
        let tx = browser_tx.lock().unwrap().take().unwrap();
        tokio::spawn(async move {
            let response = LoopbackHttp.request(get_request(&url)).await.unwrap();
            assert_eq!(response.status, 302);
            let location = response
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("location"))
                .unwrap()
                .1
                .clone();
            let mut wrong = url::Url::parse(&location).unwrap();
            let mut pairs = wrong.query_pairs().into_owned().collect::<Vec<_>>();
            set_query_pair(&mut pairs, "state", "wrong-state");
            wrong.set_query(Some(&form_body(
                pairs
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )));
            assert_eq!(
                LoopbackHttp
                    .request(get_request(wrong.as_str()))
                    .await
                    .unwrap()
                    .status,
                400
            );
            assert_eq!(
                LoopbackHttp
                    .request(get_request(&location))
                    .await
                    .unwrap()
                    .status,
                200
            );
            let _ = tx.send(());
        });
    });
    let http: Arc<dyn HttpTransport> = Arc::new(LoopbackHttp);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    let config = lingxi_core::host::McpOAuthConfigDto {
        client_id: None,
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    };
    let tokens = tokio::time::timeout(
        Duration::from_secs(5),
        perform_oauth_flow(
            &http,
            &clock,
            &config,
            "fake-server",
            &server_url,
            &on_url,
            None,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    browser_rx.await.unwrap();
    assert_eq!(tokens.access_token.expose_secret(), "fake-access");
    let meta = AuthServerMetadata {
        authorization_endpoint: format!("{base}/authorize"),
        token_endpoint: format!("{base}/token"),
        registration_endpoint: None,
        scopes_supported: None,
        scope: None,
        default_scope: None,
        revocation_endpoint: Some(format!("{base}/revoke")),
        revocation_endpoint_auth_methods_supported: None,
        token_endpoint_auth_methods_supported: Some(vec!["none".into()]),
    };
    let refreshed = refresh_standard_tokens(
        &http,
        &clock,
        &meta,
        "fake-dcr-client",
        "fake-refresh",
        &server_url,
    )
    .await
    .unwrap();
    assert_eq!(refreshed.access_token.expose_secret(), "fake-access");
    revoke_token(
        &http,
        &format!("{base}/revoke"),
        "fake-refresh",
        TokenTypeHint::RefreshToken,
        Some("fake-dcr-client"),
        None,
        Some("fake-access"),
        "client_secret_basic",
    )
    .await
    .unwrap();
    let requests = traffic.lock().unwrap();
    let paths = requests
        .iter()
        .map(|(path, _, _)| path.split('?').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server/issuer",
            "/.well-known/openid-configuration/issuer",
            "/issuer/.well-known/openid-configuration",
            "/register",
            "/authorize",
            "/token",
            "/token",
            "/revoke",
            "/revoke"
        ]
    );
    for (_, headers, _) in requests.iter().take(5) {
        assert!(headers
            .to_ascii_lowercase()
            .contains("mcp-protocol-version: 2025-11-25\r\n"));
    }
    peer.abort();
}

#[tokio::test]
async fn native_322_missing_metadata_uses_defaults_but_http_and_parse_errors_do_not() {
    for status in [401, 404, 502] {
        let capture = CaptureHttp::new(&[status; 16]);
        let http: Arc<dyn HttpTransport> = capture;
        let discovered =
            discover_auth_server_for_flow(&http, "http://localhost:31337/mcp", None, None, true)
                .await
                .unwrap();
        assert_eq!(
            discovered.metadata.authorization_endpoint,
            "http://localhost:31337/authorize"
        );
        assert_eq!(
            discovered.metadata.token_endpoint,
            "http://localhost:31337/token"
        );
        assert_eq!(
            discovered.metadata.registration_endpoint.as_deref(),
            Some("http://localhost:31337/register")
        );
        assert!(discovered.issuer.is_none());
    }
    for status in [200, 500] {
        let capture = CaptureHttp::new(&[status; 16]);
        let http: Arc<dyn HttpTransport> = capture;
        assert!(discover_auth_server_for_flow(
            &http,
            "http://localhost:31337/mcp",
            None,
            None,
            true
        )
        .await
        .is_err());
    }
    let capture = CaptureHttp::new(&[404; 16]);
    let http: Arc<dyn HttpTransport> = capture;
    assert!(
        discover_auth_server_metadata(&http, "http://localhost:31337/mcp", None, None)
            .await
            .is_err(),
        "discovery-only public API retains its documented error"
    );
}

#[tokio::test]
async fn native_322_loopback_auth_flow_uses_default_endpoints_after_all_discovery_404() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let traffic = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_traffic = traffic.clone();
    let server_base = base.clone();
    let peer = tokio::spawn(async move {
        let mut challenge = String::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, body) = read_http_message(&mut socket).await;
            let target = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let url = url::Url::parse(&format!("{server_base}{target}")).unwrap();
            server_traffic.lock().unwrap().push(url.path().to_owned());
            let (status, response, extra) = match url.path() {
                path if path.contains(".well-known/") => (404, String::new(), String::new()),
                "/register" => {
                    let mut body: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(body["client_name"], "LingXi (fake-fallback)");
                    assert!(body.get("scope").is_none());
                    body["client_id"] = json!("fake-default-client");
                    (201, body.to_string(), String::new())
                }
                "/authorize" => {
                    let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
                    challenge = query["code_challenge"].clone();
                    assert!(!query.contains_key("scope"));
                    let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
                    callback
                        .query_pairs_mut()
                        .append_pair("code", "fake-default-code")
                        .append_pair("state", &query["state"]);
                    (302, String::new(), format!("Location: {callback}\r\n"))
                }
                "/token" => {
                    let form = url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect::<BTreeMap<_, _>>();
                    assert_eq!(form["client_id"], "fake-default-client");
                    assert_eq!(
                        URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
                        challenge
                    );
                    assert_eq!(form["code"], "fake-default-code");
                    assert!(!form.contains_key("resource"));
                    (
                        200,
                        r#"{"access_token":"fake-default-access","token_type":"Bearer"}"#.into(),
                        String::new(),
                    )
                }
                path => panic!("unexpected default endpoint {path}"),
            };
            let wire = format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{response}", response.len());
            socket.write_all(wire.as_bytes()).await.unwrap();
        }
    });
    let on_url: OnAuthorizationUrl = Arc::new(|url| {
        let url = url.to_owned();
        tokio::spawn(async move {
            let response = LoopbackHttp.request(get_request(&url)).await.unwrap();
            assert_eq!(response.status, 302);
            let location = response
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("location"))
                .unwrap()
                .1
                .clone();
            assert_eq!(
                LoopbackHttp
                    .request(get_request(&location))
                    .await
                    .unwrap()
                    .status,
                200
            );
        });
    });
    let http: Arc<dyn HttpTransport> = Arc::new(LoopbackHttp);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    let config = lingxi_core::host::McpOAuthConfigDto {
        client_id: None,
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    };
    let tokens = tokio::time::timeout(
        Duration::from_secs(5),
        perform_oauth_flow(
            &http,
            &clock,
            &config,
            "fake-fallback",
            &format!("{base}/mcp"),
            &on_url,
            None,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tokens.access_token.expose_secret(), "fake-default-access");
    let requests = traffic.lock().unwrap();
    assert_eq!(
        &requests[requests.len() - 3..],
        ["/register", "/authorize", "/token"]
    );
    assert!(requests[..requests.len() - 3]
        .iter()
        .all(|path| path.contains(".well-known/")));
    peer.abort();
}
