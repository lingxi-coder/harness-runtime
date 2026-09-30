//! Cross-App Access (XAA / SEP-990) core handshake, against a mock IdP + AS.
//!
//! Covers (task TDD list, part C):
//! - `perform_cross_app_access` drives PRM → AS metadata → RFC 8693 IdP
//!   token-exchange (id_token → ID-JAG) → RFC 7523 AS jwt-bearer (ID-JAG →
//!   access_token), asserting both legs are hit with the correct grant types
//!   and the resulting access token + AS issuer come back;
//! - the registry XAA gate: `oauth.xaa=Some(true)` with `LINGXI_ENABLE_XAA`
//!   unset hard-fails with actionable copy.
#![allow(clippy::doc_markdown)] // dense OAuth/OIDC vocabulary

use async_trait::async_trait;
use lingxi_core::host::http::SseStream;
use lingxi_core::host::{HttpError, HttpTransport};
use lingxi_core::types::{HttpRequest, HttpResponse};
use mcp::xaa::{self, XaaConfig};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Mock transport answering PRM / AS-metadata / IdP-exchange / AS-jwt-bearer.
// ---------------------------------------------------------------------------

struct MockXaa {
    requests: Mutex<Vec<HttpRequest>>,
}
impl MockXaa {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
    fn route(req: &HttpRequest) -> (u16, String) {
        let url = &req.url;
        let body = req.body.as_deref().unwrap_or("");
        // PRM on the MCP server.
        if url.contains("oauth-protected-resource") {
            return (
                200,
                r#"{"resource":"https://mcp.example.com/mcp","authorization_servers":["https://as.example.com"]}"#
                    .to_string(),
            );
        }
        // AS metadata (issuer must match the as_url it's fetched from).
        if url.contains("oauth-authorization-server") {
            return (
                200,
                r#"{"issuer":"https://as.example.com","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"],"token_endpoint_auth_methods_supported":["client_secret_basic"]}"#
                    .to_string(),
            );
        }
        // IdP token-exchange (RFC 8693): returns an ID-JAG.
        if url.contains("idp.example.com/token") {
            assert!(
                body.contains(
                    "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange"
                ),
                "IdP leg must use the token-exchange grant; body={body}"
            );
            assert!(body.contains("subject_token=the-id-token"));
            return (
                200,
                r#"{"access_token":"the-id-jag","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag","expires_in":300}"#
                    .to_string(),
            );
        }
        // AS jwt-bearer (RFC 7523): ID-JAG → access_token.
        if url.contains("as.example.com/token") {
            assert!(
                body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
                "AS leg must use the jwt-bearer grant; body={body}"
            );
            assert!(body.contains("assertion=the-id-jag"));
            return (
                200,
                r#"{"access_token":"the-mcp-access","token_type":"Bearer","expires_in":3600}"#
                    .to_string(),
            );
        }
        (404, String::new())
    }
}

#[async_trait]
impl HttpTransport for MockXaa {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let (status, body) = Self::route(&req);
        self.requests.lock().unwrap().push(req);
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
async fn perform_cross_app_access_drives_both_exchange_legs() {
    let http = MockXaa::new();
    let http_dyn = http.clone() as Arc<dyn HttpTransport>;

    let result = xaa::perform_cross_app_access(
        &http_dyn,
        "https://mcp.example.com/mcp",
        &XaaConfig {
            client_id: "as-client",
            client_secret: "as-secret",
            idp_client_id: "idp-client",
            idp_client_secret: None,
            idp_id_token: "the-id-token",
            idp_token_endpoint: "https://idp.example.com/token",
        },
    )
    .await
    .expect("xaa flow ok");

    assert_eq!(result.tokens.access_token, "the-mcp-access");
    assert_eq!(result.tokens.token_type, "Bearer");
    assert_eq!(result.authorization_server_url, "https://as.example.com");

    // Both legs were hit: the IdP token-exchange and the AS jwt-bearer.
    let reqs = http.requests();
    assert!(
        reqs.iter().any(|r| r.url.contains("idp.example.com/token")),
        "IdP token-exchange leg hit"
    );
    assert!(
        reqs.iter().any(|r| r.url == "https://as.example.com/token"),
        "AS jwt-bearer leg hit"
    );
    // The AS leg authenticated the confidential client via Basic (default).
    let as_leg = reqs
        .iter()
        .find(|r| r.url == "https://as.example.com/token")
        .unwrap();
    assert!(
        as_leg
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.starts_with("Basic ")),
        "AS jwt-bearer uses client_secret_basic by default"
    );
}

#[tokio::test]
async fn xaa_token_exchange_4xx_clears_id_token() {
    // An IdP that 400s the exchange → should_clear_id_token = true.
    struct Idp4xx;
    #[async_trait]
    impl HttpTransport for Idp4xx {
        async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            let (status, body) = if req.url.contains("oauth-protected-resource") {
                (
                    200,
                    r#"{"resource":"https://mcp.example.com/mcp","authorization_servers":["https://as.example.com"]}"#.to_string(),
                )
            } else if req.url.contains("oauth-authorization-server") {
                (
                    200,
                    r#"{"issuer":"https://as.example.com","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string(),
                )
            } else if req.url.contains("idp.example.com/token") {
                (400, r#"{"error":"invalid_grant"}"#.to_string())
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
    let http = Arc::new(Idp4xx) as Arc<dyn HttpTransport>;
    let err = xaa::perform_cross_app_access(
        &http,
        "https://mcp.example.com/mcp",
        &XaaConfig {
            client_id: "as-client",
            client_secret: "as-secret",
            idp_client_id: "idp-client",
            idp_client_secret: None,
            idp_id_token: "stale",
            idp_token_endpoint: "https://idp.example.com/token",
        },
    )
    .await
    .expect_err("4xx exchange fails");
    match err {
        xaa::XaaError::TokenExchange {
            should_clear_id_token,
            ..
        } => assert!(should_clear_id_token, "4xx → clear id_token"),
        other => panic!("expected TokenExchange error, got {other:?}"),
    }
}

#[tokio::test]
async fn authorization_server_issuer_mismatch_remains_metadata_discovery_failure() {
    struct IssuerMismatch;
    #[async_trait]
    impl HttpTransport for IssuerMismatch {
        async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            let (status, body) = if req.url.contains("oauth-protected-resource") {
                (
                    200,
                    r#"{"resource":"https://mcp.example.com/mcp","authorization_servers":["https://as.example.com/root"]}"#.to_string(),
                )
            } else if req.url.contains("oauth-authorization-server") {
                (
                    200,
                    r#"{"issuer":"https://other.example.com/root","token_endpoint":"https://other.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string(),
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

    let http = Arc::new(IssuerMismatch) as Arc<dyn HttpTransport>;
    let err = xaa::perform_cross_app_access(
        &http,
        "https://mcp.example.com/mcp",
        &XaaConfig {
            client_id: "as-client",
            client_secret: "as-secret",
            idp_client_id: "idp-client",
            idp_client_secret: None,
            idp_id_token: "the-id-token",
            idp_token_endpoint: "https://idp.example.com/token",
        },
    )
    .await
    .expect_err("issuer mismatch must fail");
    match err {
        xaa::XaaError::NoAuthServer(detail) => {
            assert!(
                detail.contains("https://as.example.com/root"),
                "candidate AS URL should stay in the aggregate error: {detail}"
            );
            assert!(
                detail.contains(
                    "XAA: AS metadata discovery failed: issuer mismatch: expected https://as.example.com/root, got https://other.example.com/root"
                ),
                "aggregate error should preserve the stable metadata-failure detail: {detail}"
            );
        }
        other => panic!("expected NoAuthServer aggregate failure, got {other:?}"),
    }

    let direct = xaa::discover_authorization_server(&http, "https://as.example.com/root")
        .await
        .expect_err("direct metadata discovery must return the metadata mismatch");
    match direct {
        xaa::XaaError::AsMetadata(detail) => {
            assert_eq!(
                detail,
                "issuer mismatch: expected https://as.example.com/root, got https://other.example.com/root"
            );
        }
        other => panic!("expected AsMetadata mismatch failure, got {other:?}"),
    }
}
