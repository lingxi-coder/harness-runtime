//! `OpenAI` OAuth client: authorize URL builder, authorization-code exchange,
//! API-key mint (RFC-8693 token exchange), and proactive-refresh wiring.
//!
//! See plan spec §P2-M4. Wire shape verified against codex `login/src/server.rs`.

use crate::oauth::openai::config::OpenAiOAuthConfig;
use crate::oauth::openai::pkce::{generate_pkce, generate_state_token};
use crate::oauth::openai::refresh::{AuthState, RefreshDriver};
use platform_api::{Clock, HttpTransport};
use protocol::{HttpMethod, HttpRequest, Secret};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// Timeout for the token-exchange POST.
///
/// Codex itself sets no deadline here (`http-client/src/request.rs`,
/// `timeout: None`); 15 s is a local choice, not a matched constant. Keeping
/// the value, correcting the claim.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Render a non-2xx token-endpoint response as a diagnosable one-liner.
///
/// The raw body used to be inlined verbatim, which both leaked whatever the
/// endpoint echoed back and buried the one field that identifies the failure.
/// Anthropic's shape is `{"error":{"type":..,"message":..}}`; anything else
/// falls back to a length-capped body so an unexpected shape is still legible.
/// The full body is emitted at `debug` only.
fn describe_token_error(status: u16, body: &str) -> String {
    tracing::debug!(target: "lingxi::oauth", status, body, "token endpoint rejected the exchange");
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let kind = error
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .and_then(serde_json::Value::as_str);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| parsed.as_ref()?.get("error_description")?.as_str());
    match (kind, message) {
        (Some(kind), Some(message)) => format!("status {status} [{kind}]: {message}"),
        (Some(kind), None) => format!("status {status} [{kind}]"),
        (None, Some(message)) => format!("status {status}: {message}"),
        (None, None) => {
            let mut snippet: String = body.chars().take(200).collect();
            if body.chars().count() > 200 {
                snippet.push('\u{2026}');
            }
            format!("status {status}: {snippet}")
        }
    }
}

/// Originator value sent as an OAuth query param. Matches codex's
/// `DEFAULT_ORIGINATOR` constant (`login/src/auth/default_client.rs`).
const ORIGINATOR: &str = "codex_cli_rs";

/// OAuth-flow failures.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// Loopback callback failed (bind, parse, or state mismatch).
    #[error("callback failed: {0}")]
    Callback(String),
    /// Token exchange against the `IdP` failed.
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    /// Stored `refresh_token` has expired or was revoked.
    #[error("Session expired. Re-authenticate?")]
    RefreshExpired,
    /// Scope upgrade attempt was denied by the provider.
    #[error("Scope upgrade denied by provider")]
    ScopeRejected {
        /// Scopes the provider required.
        required: Vec<String>,
        /// Scopes the token currently holds.
        granted: Vec<String>,
    },
    /// Proactive refresh task failed and is shutting down.
    #[error("proactive refresh failed: {source}")]
    ProactiveFailed {
        /// The underlying error that caused the proactive task to fail.
        source: Box<OAuthError>,
    },
    /// Device-code flow timeout or poll failure.
    #[error("device code login failed: {0}")]
    DeviceCode(String),
}

/// Raw token-endpoint response for the `authorization_code` grant.
#[derive(Debug, Deserialize)]
pub(crate) struct ExchangeResponse {
    pub(crate) id_token: Option<String>,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_in: u64,
}

/// Tokens returned by [`OpenAiOAuthClient::exchange_code`].
#[derive(Debug)]
pub struct ExchangedTokens {
    /// Bearer access token.
    pub access_token: Secret<String>,
    /// Refresh token, if the provider issued one.
    pub refresh_token: Option<Secret<String>>,
    /// `id_token` JWT (carries `ChatGPT` `account_id` / `FedRAMP` claims).
    pub id_token: Option<String>,
    /// Wall-clock expiry instant.
    pub expires_at: SystemTime,
}

/// Real wall-clock source used as the default.
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Stateful client for the `OpenAI` Authorization Code flow.
pub struct OpenAiOAuthClient {
    pub(crate) config: OpenAiOAuthConfig,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
}

impl OpenAiOAuthClient {
    /// Construct a client with a static config and HTTP transport.
    #[must_use]
    pub fn new(config: OpenAiOAuthConfig, http: Arc<dyn HttpTransport>) -> Self {
        Self {
            config,
            http,
            clock: Arc::new(SystemClock),
        }
    }

    /// Override the clock (used by tests to make `expires_at` deterministic).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Borrow the static config.
    #[must_use]
    pub fn config(&self) -> &OpenAiOAuthConfig {
        &self.config
    }

    /// Shared HTTP transport.
    #[must_use]
    pub fn http(&self) -> Arc<dyn HttpTransport> {
        self.http.clone()
    }

    /// Build the authorize URL, returning `(url, verifier, state)`.
    ///
    /// Caller opens `url` in a browser, holds `verifier` until the loopback
    /// callback fires, and validates the redirect's `state` against the
    /// returned token.
    #[must_use]
    pub fn build_authorize_url(&self, port: u16) -> (String, String, String) {
        let redirect_uri = self.config.redirect_uri(port);
        self.build_authorize_url_with_redirect(&redirect_uri)
    }

    /// Build the authorize URL with an explicit `redirect_uri`.
    ///
    /// Query params match codex `server.rs::build_authorize_url`:
    /// - `response_type=code`, `client_id`, `redirect_uri`, scope, `code_challenge`,
    ///   `code_challenge_method=S256`, `id_token_add_organizations=true`,
    ///   `codex_cli_simplified_flow=true`, state, `originator=codex_cli_rs`
    #[must_use]
    pub fn build_authorize_url_with_redirect(
        &self,
        redirect_uri: &str,
    ) -> (String, String, String) {
        let (verifier, challenge) = generate_pkce();
        let state = generate_state_token();
        let query = vec![
            ("response_type", "code".to_string()),
            ("client_id", self.config.client_id.clone()),
            ("redirect_uri", redirect_uri.to_string()),
            ("scope", self.config.scopes.clone()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256".to_string()),
            ("id_token_add_organizations", "true".to_string()),
            ("codex_cli_simplified_flow", "true".to_string()),
            ("state", state.clone()),
            ("originator", ORIGINATOR.to_string()),
        ];
        let qs = query
            .into_iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{}?{qs}", self.config.authorize_url);
        (url, verifier, state)
    }

    /// Exchange an authorization `code` for an access + refresh token pair.
    ///
    /// POSTs the `authorization_code` grant as form-urlencoded to
    /// `config.token_url` (matching codex's `exchange_code_for_tokens` wire shape).
    ///
    /// # Errors
    /// * [`OAuthError::TokenExchange`] on transport failure, non-200 status,
    ///   or undecodable body. A `401` maps to the locked "Invalid authorization
    ///   code" message.
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        port: u16,
    ) -> Result<ExchangedTokens, OAuthError> {
        let redirect_uri = self.config.redirect_uri(port);
        self.exchange_code_with_redirect(code, verifier, &redirect_uri)
            .await
    }

    /// Exchange a code with an explicit `redirect_uri`.
    pub async fn exchange_code_with_redirect(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<ExchangedTokens, OAuthError> {
        // Wire: form-urlencoded, matching codex server.rs exchange_code_for_tokens.
        let body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding::encode(code),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(&self.config.client_id),
            urlencoding::encode(verifier),
        );
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: self.config.token_url.clone(),
            headers: vec![
                (
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
                ("accept".into(), "application/json".into()),
            ],
            body: Some(body),
            body_bytes: None,
            timeout: Some(EXCHANGE_TIMEOUT),
        };
        let resp = self
            .http
            .request(req)
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("transport: {e}")))?;
        let parsed: ExchangeResponse = match resp.status {
            200 => serde_json::from_str(&resp.body)
                .map_err(|e| OAuthError::TokenExchange(format!("decode: {e}")))?,
            401 => {
                return Err(OAuthError::TokenExchange(
                    "Authentication failed: Invalid authorization code".into(),
                ))
            }
            other => {
                return Err(OAuthError::TokenExchange(describe_token_error(
                    other, &resp.body,
                )))
            }
        };

        let expires_in = if parsed.expires_in == 0 {
            // Default to 1 hour if missing.
            3600
        } else {
            parsed.expires_in
        };
        let expires_at = self.clock.now() + Duration::from_secs(expires_in);

        Ok(ExchangedTokens {
            access_token: Secret::new(parsed.access_token),
            refresh_token: parsed.refresh_token.map(Secret::new),
            id_token: parsed.id_token,
            expires_at,
        })
    }

    /// Mint an API key via RFC-8693 token exchange.
    ///
    /// POSTs to `config.token_url` form-urlencoded with:
    /// - grant_type=urn:ietf:params:oauth:grant-type:token-exchange
    /// - `client_id`
    /// - requested_token=openai-api-key
    /// - `subject_token`=<`id_token`>
    /// - subject_token_type=urn:ietf:params:oauth:token-type:id_token
    ///
    /// Returns the `access_token` field from the response.
    ///
    /// # Errors
    /// * [`OAuthError::TokenExchange`] on transport failure, non-2xx status,
    ///   or undecodable body.
    pub async fn obtain_api_key(&self, id_token: &str) -> Result<String, OAuthError> {
        #[derive(Deserialize)]
        struct ExchangeResp {
            access_token: String,
        }

        let body = format!(
            "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
            urlencoding::encode("urn:ietf:params:oauth:grant-type:token-exchange"),
            urlencoding::encode(&self.config.client_id),
            urlencoding::encode("openai-api-key"),
            urlencoding::encode(id_token),
            urlencoding::encode("urn:ietf:params:oauth:token-type:id_token"),
        );
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: self.config.token_url.clone(),
            headers: vec![
                (
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
                ("accept".into(), "application/json".into()),
            ],
            body: Some(body),
            body_bytes: None,
            timeout: Some(EXCHANGE_TIMEOUT),
        };
        let resp = self
            .http
            .request(req)
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("transport: {e}")))?;
        if resp.status != 200 {
            return Err(OAuthError::TokenExchange(format!(
                "api key exchange status {}: {}",
                resp.status, resp.body
            )));
        }
        let parsed: ExchangeResp = serde_json::from_str(&resp.body)
            .map_err(|e| OAuthError::TokenExchange(format!("decode: {e}")))?;
        Ok(parsed.access_token)
    }
}

/// Wire the OAuth refresh subsystem: construct `AuthState`, and spawn the
/// proactive task.
///
/// Returns the `Arc<AuthState>` so the engine can call `shutdown()` at
/// `Engine::shutdown` time.
///
/// # Errors
/// * [`OAuthError::TokenExchange`] if the runtime spawner fails to spawn.
#[allow(clippy::too_many_arguments)]
pub async fn init_refresh_driver(
    config: OpenAiOAuthConfig,
    access_token: Secret<String>,
    refresh_token: Option<Secret<String>>,
    expires_at: SystemTime,
    account_id: Option<String>,
    fedramp: bool,
    email: Option<String>,
    http: Arc<dyn platform_api::HttpTransport>,
    clock: Arc<dyn platform_api::Clock>,
    bus: Option<Arc<telemetry::AnalyticsBus>>,
    credentials: Option<Arc<secret::CredentialManager>>,
    spawner: Arc<dyn platform_api::RuntimeSpawner>,
) -> Result<Arc<AuthState>, OAuthError> {
    let state = AuthState::new(
        config,
        access_token,
        refresh_token,
        expires_at,
        account_id,
        fedramp,
        email,
        http,
        clock,
        bus,
        credentials,
    );
    RefreshDriver::spawn_proactive(state.clone(), spawner)
        .await
        .map_err(|e| OAuthError::TokenExchange(format!("spawn_proactive: {e}")))?;
    Ok(state)
}

#[cfg(test)]
mod exchange_tests {
    use super::*;
    use crate::oauth::openai::testsupport::{Canned, MockHttp, TestClock};

    fn client_with(http: Arc<MockHttp>, clock_secs: u64) -> OpenAiOAuthClient {
        let clock = TestClock::new(clock_secs);
        let cfg = OpenAiOAuthConfig::default();
        OpenAiOAuthClient::new(cfg, http as Arc<dyn HttpTransport>).with_clock(clock)
    }

    #[tokio::test]
    async fn exchange_code_posts_form_and_parses_tokens() {
        let body = r#"{
            "id_token": "hdr.eyJlbWFpbCI6InVAZXhhbXBsZS5jb20ifQ.sig",
            "access_token": "acc-1",
            "refresh_token": "ref-1",
            "expires_in": 3600
        }"#;
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: body.into(),
            },
        )]);
        let client = client_with(http.clone(), 1_000);

        let tokens = client
            .exchange_code_with_redirect(
                "the-code",
                "the-verifier",
                "http://localhost:1455/auth/callback",
            )
            .await
            .expect("exchange ok");

        assert_eq!(tokens.access_token.expose_secret(), "acc-1");
        assert_eq!(
            tokens
                .refresh_token
                .as_ref()
                .map(|s| s.expose_secret().clone()),
            Some("ref-1".to_string())
        );
        // expires_at = clock.now() (1000s) + 3600s
        assert_eq!(
            tokens.expires_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + 3_600)
        );
        assert!(tokens.id_token.is_some());

        // Assert the wire shape: POST, form-urlencoded content-type.
        let req = http.last_request().expect("a request was made");
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-www-form-urlencoded"));
        let sent = req.body.as_deref().unwrap();
        assert!(sent.contains("grant_type=authorization_code"));
        assert!(sent.contains("code=the-code"));
        assert!(sent.contains("code_verifier=the-verifier"));
        assert!(sent.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert_eq!(http.call_count(), 1);
    }

    #[tokio::test]
    async fn exchange_code_401_maps_to_invalid_code_message() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let client = client_with(http, 0);
        let err = client
            .exchange_code_with_redirect("bad", "v", "http://localhost:1455/auth/callback")
            .await
            .expect_err("401 must error");
        match err {
            OAuthError::TokenExchange(msg) => {
                assert_eq!(msg, "Authentication failed: Invalid authorization code");
            }
            other => panic!("expected TokenExchange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_authorize_url_includes_required_params() {
        let http = MockHttp::new(vec![]);
        let cfg = OpenAiOAuthConfig::default();
        let client = OpenAiOAuthClient::new(cfg, http as Arc<dyn HttpTransport>);
        let (url, _verifier, state) =
            client.build_authorize_url_with_redirect("http://localhost:1455/auth/callback");

        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("originator=codex_cli_rs"));
        assert!(url.contains(&format!("state={}", urlencoding::encode(&state))));
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    }

    #[tokio::test]
    async fn obtain_api_key_posts_rfc8693_exchange() {
        let api_key_resp = r#"{"access_token":"sk-openai-key-123"}"#;
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: api_key_resp.into(),
            },
        )]);
        let client = client_with(http.clone(), 0);

        let key = client
            .obtain_api_key("fake-id-token")
            .await
            .expect("api key exchange ok");

        assert_eq!(key, "sk-openai-key-123");

        let req = http.last_request().expect("request was made");
        let sent = req.body.as_deref().unwrap();
        assert!(
            sent.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange")
        );
        assert!(sent.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(sent.contains("requested_token=openai-api-key"));
        assert!(sent.contains("subject_token=fake-id-token"));
        assert!(
            sent.contains("subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aid_token")
        );
    }

    #[tokio::test]
    async fn obtain_api_key_non_200_maps_to_error() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 403,
                body: r#"{"error":"access_denied"}"#.into(),
            },
        )]);
        let client = client_with(http, 0);
        let err = client
            .obtain_api_key("fake-id-token")
            .await
            .expect_err("403 must error");
        match err {
            OAuthError::TokenExchange(msg) => {
                assert!(msg.contains("403"));
            }
            other => panic!("expected TokenExchange, got {other:?}"),
        }
    }
}
