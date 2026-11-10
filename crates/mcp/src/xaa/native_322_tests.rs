use super::*;
use lingxi_core::host::HttpError;
use lingxi_core::types::HttpResponse;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/mcp_xaa_2_1_322.json")).unwrap()
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap()
}

struct Capture {
    requests: Mutex<Vec<HttpRequest>>,
    response: HttpResponse,
}
impl Capture {
    fn new(status: u16, body: &str) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            response: HttpResponse {
                status,
                body: body.into(),
                headers: Vec::new(),
                body_bytes: Vec::new(),
            },
        })
    }
}
#[async_trait::async_trait]
impl HttpTransport for Capture {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request);
        Ok(self.response.clone())
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::Connection("unused".into()))
    }
}

enum Reply {
    Exchange(JwtAuthGrantResult),
    Bearer(XaaTokenResult),
}
impl Reply {
    fn source(&self) -> &Utf16JsonProjection {
        match self {
            Self::Exchange(value) => &value.source,
            Self::Bearer(value) => &value.source,
        }
    }
    fn expiry(&self) -> Option<f64> {
        match self {
            Self::Exchange(value) => value.expires_in,
            Self::Bearer(value) => value.expires_in,
        }
    }
}

fn input(kind: &str) -> Value {
    if kind == "exchange" {
        json!({"tokenEndpoint":"https://idp.example/token","audience":"https://as.example/issuer","resource":"https://mcp.example/mcp","idToken":"fake +~!*id-token","clientId":"fake +~!*client","scope":"ignored scope"})
    } else {
        json!({"tokenEndpoint":"https://as.example/token","assertion":"fake +~!*assertion","clientId":"fake +~!*client","clientSecret":"fake +~!*é","scope":"ignored scope"})
    }
}

async fn call(kind: &str, input: &Value, http: &Arc<dyn HttpTransport>) -> Result<Reply, XaaError> {
    if kind == "exchange" {
        request_jwt_authorization_grant(
            http,
            &JwtAuthGrantRequest {
                token_endpoint: text(input, "tokenEndpoint"),
                audience: text(input, "audience"),
                resource: text(input, "resource"),
                id_token: text(input, "idToken"),
                client_id: text(input, "clientId"),
                client_secret: input["clientSecret"].as_str(),
                scope: input["scope"].as_str(),
            },
        )
        .await
        .map(Reply::Exchange)
    } else {
        exchange_jwt_auth_grant(
            http,
            &JwtBearerRequest {
                token_endpoint: text(input, "tokenEndpoint"),
                assertion: text(input, "assertion"),
                client_id: text(input, "clientId"),
                client_secret: text(input, "clientSecret"),
                auth_method: if input["authMethod"] == "client_secret_post" {
                    AuthMethod::ClientSecretPost
                } else {
                    AuthMethod::ClientSecretBasic
                },
                scope: input["scope"].as_str(),
            },
        )
        .await
        .map(Reply::Bearer)
    }
}

#[test]
fn native_322_xaa_url_and_redaction_bytes() {
    let fixture = fixture();
    for case in fixture["normalizeCases"].as_array().unwrap() {
        assert_eq!(normalize_url(text(case, "input")), text(case, "xaa"));
        assert_eq!(
            crate::xaa_idp::issuer_key(text(case, "input")),
            text(case, "issuer")
        );
    }
    for case in fixture["redactionCases"].as_array().unwrap() {
        assert_eq!(redact_tokens(text(case, "input")), text(case, "expected"));
    }
}

#[tokio::test]
async fn native_322_xaa_form_header_and_coerced_response_bytes() {
    let fixture = fixture();
    for case in fixture["wireCases"].as_array().unwrap() {
        let kind = text(case, "kind");
        let response = if kind == "exchange" {
            r#"{"access_token":"fake-id-jag","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag","expires_in":"1.25","extra":"strip"}"#
        } else {
            r#"{"access_token":"fake-access","expires_in":"1.25","extra":"strip"}"#
        };
        let capture = Capture::new(200, response);
        let http: Arc<dyn HttpTransport> = capture.clone();
        let result = call(kind, &case["input"], &http).await.unwrap();
        assert_eq!(result.expiry(), Some(1.25));
        let request = capture.requests.lock().unwrap()[0].clone();
        assert_eq!(request.url, text(&case["request"], "url"));
        assert_eq!(
            request.body.as_deref(),
            Some(text(&case["request"], "body"))
        );
        assert_eq!(
            serde_json::to_value(
                request
                    .headers
                    .into_iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value))
                    .collect::<BTreeMap<_, _>>()
            )
            .unwrap(),
            case["request"]["headers"]
        );
    }
}

#[tokio::test]
async fn native_322_xaa_failure_diagnostics_and_id_token_invalidation() {
    let fixture = fixture();
    for case in fixture["responseCases"].as_array().unwrap() {
        let kind = text(case, "kind");
        let http: Arc<dyn HttpTransport> =
            Capture::new(case["status"].as_u64().unwrap() as u16, text(case, "body"));
        let result = call(kind, &input(kind), &http).await;
        assert_eq!(result.is_err(), case.get("error").is_some(), "{case}");
        if let Err(error) = result {
            let expected = case["error"]["message_units"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_u64().unwrap() as u16)
                .collect::<Vec<_>>();
            assert_eq!(
                error.source_diagnostic().string_units("").unwrap(),
                expected,
                "{case}"
            );
            if let Some(expected) = case["error"]["should_clear"].as_bool() {
                assert_eq!(error.should_clear_id_token(), expected);
            }
            let wire: lingxi_core::host::McpError = error.into();
            assert_eq!(
                wire.source_diagnostic().unwrap().string_units("").unwrap(),
                expected
            );
        }
    }
}

#[tokio::test]
async fn native_322_xaa_token_source_preserves_lone_utf16() {
    let fixture = fixture();
    for case in fixture["sourceCases"].as_array().unwrap() {
        let kind = text(case, "kind");
        let http: Arc<dyn HttpTransport> = Capture::new(200, text(case, "body"));
        let result = call(kind, &input(kind), &http).await.unwrap();
        assert_eq!(
            result.source().to_json_string().unwrap(),
            text(case, "expected_json")
        );
    }
}

#[tokio::test]
async fn native_322_xaa_pinned_issuer_prevents_credentials_crossing_to_changed_as() {
    let capture = Capture::new(
        200,
        r#"{"resource":"https://mcp.example/mcp","authorization_servers":["https://new-as.example"]}"#,
    );
    let http: Arc<dyn HttpTransport> = capture.clone();
    let result = perform_cross_app_access_with_issuer(
        &http,
        "https://mcp.example/mcp",
        &XaaConfig {
            client_id: "fake-client",
            client_secret: "fake-secret",
            idp_client_id: "fake-idp",
            idp_client_secret: None,
            idp_id_token: "fake-id-token",
            idp_token_endpoint: "https://idp.example/token",
        },
        Some("https://old-as.example"),
    )
    .await;
    assert!(
        matches!(result,Err(XaaError::AsMetadata(message)) if message.contains("authorization server changed"))
    );
    let requests = capture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.is_none());
    assert!(!requests[0]
        .headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("authorization")));
}

#[tokio::test]
async fn native_322_xaa_basic_and_post_distinguish_unpaired_from_literal_replacement() {
    let fixture = fixture();
    for case in fixture["secretSourceCases"].as_array().unwrap() {
        let secret = Utf16JsonProjection::parse(text(case, "source_json")).unwrap();
        let capture = Capture::new(200, r#"{"access_token":"fake-access"}"#);
        let http: Arc<dyn HttpTransport> = capture.clone();
        let request = JwtBearerRequest {
            token_endpoint: "https://as.example/token",
            assertion: "fake +~!*assertion",
            client_id: "fake +~!*client",
            client_secret: secret.string_at("").unwrap(),
            auth_method: if case["method"] == "client_secret_post" {
                AuthMethod::ClientSecretPost
            } else {
                AuthMethod::ClientSecretBasic
            },
            scope: Some("ignored scope"),
        };
        let result = exchange_jwt_auth_grant_with_source(&http, &request, Some(&secret)).await;
        assert_eq!(result.is_err(), case.get("error_name").is_some());
        let seen = capture.requests.lock().unwrap();
        if let Some(expected) = case.get("request") {
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].body.as_deref(), Some(text(expected, "body")));
            assert_eq!(
                serde_json::to_value(
                    seen[0]
                        .headers
                        .iter()
                        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
                        .collect::<BTreeMap<_, _>>()
                )
                .unwrap(),
                expected["headers"]
            );
        } else {
            assert!(
                seen.is_empty(),
                "unpaired basic secret must fail beforeHTTP"
            );
        }
    }
}

#[tokio::test]
async fn native_322_xaa_metadata_projects_wire_strings_and_keeps_exact_issuer() {
    let http: Arc<dyn HttpTransport> = Capture::new(
        200,
        r#"{"issuer":"https://as.example/\ud800","authorization_endpoint":"https://as.example/authorize","token_endpoint":"https://as.example/\udfff","response_types_supported":["code"],"grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#,
    );
    let (metadata, source) =
        discover_authorization_server_source(&http, "https://as.example/\u{FFFD}")
            .await
            .unwrap();
    assert_eq!(metadata.issuer, "https://as.example/\u{FFFD}");
    assert_eq!(metadata.token_endpoint, "https://as.example/\u{FFFD}");
    assert_eq!(
        source.to_json_string().unwrap(),
        r#""https://as.example/\ud800""#
    );
    let http: Arc<dyn HttpTransport> = Capture::new(
        200,
        r#"{"resource":"https://mcp.example/\ud800","authorization_servers":["https://as.example/\udfff"]}"#,
    );
    let metadata = discover_protected_resource(&http, "https://mcp.example/\u{FFFD}")
        .await
        .unwrap();
    assert_eq!(
        metadata.authorization_servers,
        vec!["https://as.example/\u{FFFD}"]
    );
}
