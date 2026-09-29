//! Host lifecycle adapter for OpenAI OAuth. Wire operations live in llm-client.
use crate::oauth::openai::refresh::{AuthState, RefreshDriver};
use lingxi_llm_client::auth::oauth::openai as sdk;
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use lingxi_llm_client::Transport;
use platform_api::Clock;
use protocol::Secret;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("callback failed: {0}")]
    Callback(String),
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    #[error("Session expired. Re-authenticate?")]
    RefreshExpired,
    #[error("Scope upgrade denied by provider")]
    ScopeRejected {
        required: Vec<String>,
        granted: Vec<String>,
    },
    #[error("proactive refresh failed: {source}")]
    ProactiveFailed { source: Box<OAuthError> },
    #[error("device code login failed: {0}")]
    DeviceCode(String),
}

#[derive(Debug)]
pub struct ExchangedTokens {
    pub access_token: Secret<String>,
    pub refresh_token: Option<Secret<String>>,
    pub id_token: Option<String>,
    pub expires_at: SystemTime,
}

struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

pub struct OpenAiOAuthClient {
    config: OpenAiOAuthConfig,
    http: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
}
impl OpenAiOAuthClient {
    pub fn new(config: OpenAiOAuthConfig, http: Arc<dyn Transport>) -> Self {
        Self {
            config,
            http,
            clock: Arc::new(SystemClock),
        }
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    pub fn config(&self) -> &OpenAiOAuthConfig {
        &self.config
    }
    pub fn http(&self) -> Arc<dyn Transport> {
        self.http.clone()
    }
    pub fn build_authorize_url(&self, port: u16) -> (String, String, String) {
        self.config.build_authorize_url(port)
    }
    pub fn build_authorize_url_with_redirect(
        &self,
        redirect_uri: &str,
    ) -> (String, String, String) {
        self.config.build_authorize_url_with_redirect(redirect_uri)
    }
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        port: u16,
    ) -> Result<ExchangedTokens, OAuthError> {
        self.exchange_code_with_redirect(code, verifier, &self.config.redirect_uri(port))
            .await
    }
    pub async fn exchange_code_with_redirect(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<ExchangedTokens, OAuthError> {
        let token = sdk::exchange_code(
            self.http.as_ref(),
            &self.config,
            code,
            verifier,
            redirect_uri,
        )
        .await
        .map_err(|e| match e {
            sdk::OAuthProtocolError::Status(401) => OAuthError::TokenExchange(
                "Authentication failed: Invalid authorization code".into(),
            ),
            other => OAuthError::TokenExchange(other.to_string()),
        })?;
        let expires_in = if token.expires_in == 0 {
            3600
        } else {
            token.expires_in
        };
        Ok(ExchangedTokens {
            access_token: Secret::new(token.access_token),
            refresh_token: token.refresh_token.map(Secret::new),
            id_token: token.id_token,
            expires_at: self.clock.now() + Duration::from_secs(expires_in),
        })
    }
    pub async fn obtain_api_key(&self, id_token: &str) -> Result<String, OAuthError> {
        sdk::obtain_api_key(self.http.as_ref(), &self.config, id_token)
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))
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
    http: Arc<dyn lingxi_llm_client::Transport>,
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
        OpenAiOAuthClient::new(cfg, http as Arc<dyn lingxi_llm_client::Transport>).with_clock(clock)
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
        assert_eq!(req.method, "POST");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-www-form-urlencoded"));
        let sent = std::str::from_utf8(&req.body).unwrap();
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
        let client = OpenAiOAuthClient::new(cfg, http as Arc<dyn lingxi_llm_client::Transport>);
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
        let sent = std::str::from_utf8(&req.body).unwrap();
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
