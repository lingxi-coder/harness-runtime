//! Claude.ai OAuth client skeleton.
//!
//! See spec §30. M1.19 ships the contract + PKCE/state generation + the
//! authorize URL builder; browser-open + token exchange land with the
//! cli-demo in Plan 16.

use ::url::form_urlencoded;

use crate::oauth::anthropic::config::ClaudeAiOAuthConfig;
use crate::oauth::anthropic::pkce::{generate_pkce, generate_state_token};
use crate::oauth::anthropic::refresh::{AuthState, RefreshDriver};
use platform_api::{Clock, HttpTransport};
use protocol::{HttpMethod, HttpRequest, Secret};
use secret::CredentialManager;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// Timeout for the token-exchange POST.
///
/// The oracle posts `TOKEN_URL` with `{timeout:30000}` (2.1.241, the `kba`
/// token-exchange helper), so the deadline is 30 s — the old comment asserted
/// 15 s and the constant matched the comment rather than the oracle.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// OAuth-flow failures.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// Loopback callback failed (bind, parse, or state mismatch).
    #[error("callback failed: {0}")]
    Callback(String),
    /// Token exchange against the `IdP` failed.
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    /// Stored `refresh_token` has expired or was revoked. User must re-authenticate.
    ///
    /// OAUTHREF.5: this Display string is a LingXi-specific message, NOT a
    /// claude-code byte-for-byte port (an earlier comment wrongly claimed it
    /// was). claude-code's refresh path throws a generic
    /// `Token refresh failed: ${statusText}` (services/oauth/client.ts) and has
    /// no "Session expired" / re-auth string anywhere. This string also serves
    /// as a control-flow key in the proactive-refresh loop (`refresh.rs`), which
    /// is itself a LingXi-only redesign with no TS counterpart — so the string
    /// is pinned as a LingXi-side value, not as a TS-parity target.
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
}

/// Account record optionally embedded in the token-exchange response.
///
/// Field name is `email_address` here (the profile endpoint uses `email`).
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeAccount {
    /// Stable account UUID.
    #[serde(default)]
    pub uuid: String,
    /// User's email address.
    #[serde(default)]
    pub email_address: String,
}

/// Organization record optionally embedded in the token-exchange response.
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeOrganization {
    /// Stable organization UUID.
    #[serde(default)]
    pub uuid: String,
}

/// Raw token-endpoint response shape for the `authorization_code` grant.
///
/// `account` / `organization` are optional: when present they let us resolve
/// the user's email + org without a second `/profile` round-trip.
#[derive(Debug, Clone, Deserialize)]
struct ExchangeResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: u64,
    scope: Option<String>,
    account: Option<ExchangeAccount>,
    organization: Option<ExchangeOrganization>,
}

/// JSON request body for the `authorization_code` grant. Field order matches
/// claude-code's `exchangeCodeForTokens`.
#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in: Option<u64>,
}

/// Optional authorize-URL fields used by the public `auth login` and
/// `setup-token` flows.
#[derive(Debug, Clone, Default)]
pub struct AuthorizeOptions {
    /// Override the configured scopes (used by inference-only setup tokens).
    pub scopes: Option<Vec<String>>,
    /// Managed organization pin forwarded to the authorization service.
    pub org_uuid: Option<String>,
    /// Pre-populated account email.
    pub login_hint: Option<String>,
    /// Login method hint such as `sso`.
    pub login_method: Option<String>,
}

/// The two authorize-URL variants of one grant, plus the PKCE verifier + state
/// they share. See [`ClaudeAiOAuthClient::build_authorize_url_pair_with_options`].
#[derive(Debug, Clone)]
pub struct AuthorizeUrlPair {
    /// Redirects to the loopback listener (`http://localhost:{port}/callback`).
    pub automatic_url: String,
    /// Redirects to the hosted code page that displays `code#state` for paste.
    pub manual_url: String,
    /// PKCE verifier for the eventual token exchange.
    pub verifier: String,
    /// CSRF state token embedded in both URLs.
    pub state: String,
}

/// Tokens (plus optionally-resolved identity) returned by [`ClaudeAiOAuthClient::exchange_code`].
#[derive(Debug)]
pub struct ExchangedTokens {
    /// Bearer access token.
    pub access_token: Secret<String>,
    /// Refresh token, if the provider issued one.
    pub refresh_token: Option<Secret<String>>,
    /// Wall-clock expiry instant (`clock.now() + expires_in`).
    pub expires_at: SystemTime,
    /// Granted scopes (parsed from the response `scope`, else the configured set).
    pub scopes: Vec<String>,
    /// Account record echoed by the token endpoint (if any).
    pub account: Option<ExchangeAccount>,
    /// Organization record echoed by the token endpoint (if any).
    pub organization: Option<ExchangeOrganization>,
}

/// Real wall-clock source used as the default for [`ClaudeAiOAuthClient`].
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Stateful client for the Claude.ai Authorization Code flow.
pub struct ClaudeAiOAuthClient {
    config: ClaudeAiOAuthConfig,
    http: Arc<dyn HttpTransport>,
    #[allow(dead_code)]
    credentials: Arc<CredentialManager>,
    clock: Arc<dyn Clock>,
}

/// Percent-encode one query value the way `URLSearchParams` does.
///
/// The oracle assembles its authorize URL through `URLSearchParams` /
/// `URL.searchParams.set`, which serialises as
/// `application/x-www-form-urlencoded` — SPACE becomes `+`, not `%20`. The
/// scope parameter is a space-joined list, so this is the one place the two
/// encodings visibly disagree, and an authorization server that compares the
/// scope string byte-for-byte would see a different value.
fn form_encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

impl ClaudeAiOAuthClient {
    /// Construct a client with a static config, HTTP transport, and credential store.
    ///
    /// Uses a real system clock for expiry computation; tests override it with
    /// [`Self::with_clock`].
    #[must_use]
    pub fn new(
        config: ClaudeAiOAuthConfig,
        http: Arc<dyn HttpTransport>,
        credentials: Arc<CredentialManager>,
    ) -> Self {
        Self {
            config,
            http,
            credentials,
            clock: Arc::new(SystemClock),
        }
    }

    /// Override the clock (used by tests to make `expires_at` deterministic).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Borrow the static config (endpoints / `client_id` / redirect / scopes).
    #[must_use]
    pub fn config(&self) -> &ClaudeAiOAuthConfig {
        &self.config
    }

    /// Shared HTTP transport — used by the login flow for the optional profile
    /// fetch.
    #[must_use]
    pub fn http(&self) -> Arc<dyn HttpTransport> {
        self.http.clone()
    }

    /// Shared credential store — used by the login flow for token persistence
    /// and by `current_user` / `logout`.
    #[must_use]
    pub fn credentials(&self) -> Arc<CredentialManager> {
        self.credentials.clone()
    }

    /// Build the authorize URL, returning `(url, verifier, state)`.
    ///
    /// Caller opens `url` in a browser, holds `verifier` until the loopback
    /// callback fires, and validates the redirect's `state` against the
    /// returned token.
    #[must_use]
    pub fn build_authorize_url(&self) -> (String, String, String) {
        self.build_authorize_url_with_redirect(&self.config.redirect_uri)
    }

    /// Build the authorize URL with an explicit `redirect_uri`.
    ///
    /// The loopback port is only known after the listener binds (it may be
    /// OS-assigned), so the interactive flow resolves the port at runtime and
    /// threads the real `redirect_uri` through both this call and
    /// [`Self::exchange_code_with_redirect`] so the two agree (OAuth requires
    /// the exchange `redirect_uri` to match the authorize one).
    #[must_use]
    pub fn build_authorize_url_with_redirect(
        &self,
        redirect_uri: &str,
    ) -> (String, String, String) {
        self.build_authorize_url_with_options(redirect_uri, &AuthorizeOptions::default())
    }

    /// Build an authorize URL with the optional fields exposed by Claude
    /// Code's public authentication commands.
    #[must_use]
    pub fn build_authorize_url_with_options(
        &self,
        redirect_uri: &str,
        options: &AuthorizeOptions,
    ) -> (String, String, String) {
        let (verifier, challenge) = generate_pkce();
        let state = generate_state_token();
        let url = self.format_authorize_url(redirect_uri, &challenge, &state, options);
        (url, verifier, state)
    }

    /// Build BOTH authorize-URL variants for one PKCE/state grant, mirroring
    /// the oracle's `lno({...o, isManual})` pair: the AUTOMATIC variant
    /// redirects to the loopback listener (`redirect_uri`), the MANUAL variant
    /// to the hosted code page ([`ClaudeAiOAuthConfig::manual_redirect_uri`])
    /// that displays `code#state` for copy-paste. Sharing one verifier + state
    /// lets either landing complete the same flow.
    #[must_use]
    pub fn build_authorize_url_pair_with_options(
        &self,
        redirect_uri: &str,
        options: &AuthorizeOptions,
    ) -> AuthorizeUrlPair {
        let (verifier, challenge) = generate_pkce();
        let state = generate_state_token();
        AuthorizeUrlPair {
            automatic_url: self.format_authorize_url(redirect_uri, &challenge, &state, options),
            manual_url: self.format_authorize_url(
                &self.config.manual_redirect_uri,
                &challenge,
                &state,
                options,
            ),
            verifier,
            state,
        }
    }

    /// Serialize one authorize URL for an already-generated PKCE challenge +
    /// state (shared by the single-URL and pair builders above).
    fn format_authorize_url(
        &self,
        redirect_uri: &str,
        challenge: &str,
        state: &str,
        options: &AuthorizeOptions,
    ) -> String {
        let scopes = options
            .scopes
            .as_ref()
            .unwrap_or(&self.config.scopes)
            .join(" ");
        let mut url = format!(
            "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
            self.config.authorization_endpoint,
            form_encode(&self.config.client_id),
            form_encode(redirect_uri),
            form_encode(&scopes),
            form_encode(challenge),
            form_encode(state),
        );
        if let Some(org_uuid) = options.org_uuid.as_deref() {
            url.push_str("&orgUUID=");
            url.push_str(&form_encode(org_uuid));
        }
        if let Some(login_hint) = options.login_hint.as_deref() {
            url.push_str("&login_hint=");
            url.push_str(&form_encode(login_hint));
        }
        if let Some(login_method) = options.login_method.as_deref() {
            url.push_str("&login_method=");
            // `form_encode`, like every other param: the oracle builds the whole
            // URL through `URLSearchParams`, which is form-urlencoded. Latent
            // today (the only value is "sso"), but the two encodings disagree on
            // SPACE.
            url.push_str(&form_encode(login_method));
        }
        url
    }

    /// Exchange an authorization `code` for an access + refresh token pair.
    ///
    /// POSTs the `authorization_code` grant as JSON to `config.token_endpoint`
    /// (matching claude-code's `exchangeCodeForTokens` wire shape), with the
    /// `redirect_uri` bound to whatever this client's config advertised in the
    /// authorize URL.
    ///
    /// # Errors
    /// * [`OAuthError::TokenExchange`] on transport failure, a non-200 status,
    ///   or an undecodable body. A `401` maps to the locked
    ///   `"Authentication failed: Invalid authorization code"` message.
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
    ) -> Result<ExchangedTokens, OAuthError> {
        self.exchange_code_with_redirect(code, verifier, state, &self.config.redirect_uri)
            .await
    }

    /// Exchange a code using an explicit `redirect_uri` (must match the one in
    /// the authorize URL). See [`Self::build_authorize_url_with_redirect`].
    ///
    /// # Errors
    /// Same as [`Self::exchange_code`].
    pub async fn exchange_code_with_redirect(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
    ) -> Result<ExchangedTokens, OAuthError> {
        self.exchange_code_with_options(code, verifier, state, redirect_uri, None)
            .await
    }

    /// Exchange a code while optionally requesting a provider-defined token
    /// lifetime. `setup-token` uses one year; normal login omits the field.
    pub async fn exchange_code_with_options(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
        expires_in: Option<u64>,
    ) -> Result<ExchangedTokens, OAuthError> {
        let body = ExchangeRequest {
            grant_type: "authorization_code",
            code,
            redirect_uri,
            client_id: &self.config.client_id,
            code_verifier: verifier,
            state,
            expires_in,
        };
        let body = serde_json::to_string(&body)
            .map_err(|e| OAuthError::TokenExchange(format!("encode: {e}")))?;
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: self.config.token_endpoint.clone(),
            headers: vec![
                ("content-type".into(), "application/json".into()),
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

        let scopes = parsed.scope.as_deref().map_or_else(
            || self.config.scopes.clone(),
            |s| s.split_whitespace().map(str::to_string).collect::<Vec<_>>(),
        );
        let expires_at = self.clock.now() + Duration::from_secs(parsed.expires_in);

        Ok(ExchangedTokens {
            access_token: Secret::new(parsed.access_token),
            refresh_token: parsed.refresh_token.map(Secret::new),
            expires_at,
            scopes,
            account: parsed.account,
            organization: parsed.organization,
        })
    }
}

/// Wire the OAuth refresh subsystem: construct `AuthState`, register the
/// `OAuthRefreshHook` impl process-globally, and spawn the proactive task.
///
/// Returns the `Arc<AuthState>` so the engine can call `shutdown()` at
/// `Engine::shutdown` time. Re-calling `init_refresh_driver` is a bug
/// (process-global hook is already registered); the second call logs a WARN
/// and returns the freshly-constructed `AuthState` so callers see consistent
/// behaviour but stale token data won't be auto-refreshed (the original
/// proactive task is still running on the original state).
///
/// # Errors
/// * [`OAuthError::TokenExchange`] if the runtime spawner fails to spawn.
#[allow(clippy::too_many_arguments)]
pub async fn init_refresh_driver(
    config: ClaudeAiOAuthConfig,
    access_token: Secret<String>,
    refresh_token: Option<Secret<String>>,
    expires_at: SystemTime,
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
        http,
        clock,
        bus,
        credentials,
    );
    // Spawn the proactive refresh loop. The api-client hook seam
    // (register_oauth_hook / current_hook) was removed in Plan 3a; the live
    // model path runs entirely through llm-runtime. The WebSearch builder
    // uses an API key (not OAuth) and never triggers the 401-refresh path.
    RefreshDriver::spawn_proactive(state.clone(), spawner)
        .await
        .map_err(|e| OAuthError::TokenExchange(format!("spawn_proactive: {e}")))?;
    Ok(state)
}

#[cfg(test)]
mod exchange_tests {
    use super::*;
    use crate::oauth::anthropic::testsupport::{
        mem_credential_manager, Canned, MemStorage, MockHttp, TestClock,
    };

    fn client_with(http: Arc<MockHttp>, clock_secs: u64) -> ClaudeAiOAuthClient {
        let clock = TestClock::new(clock_secs);
        let cm = mem_credential_manager(MemStorage::new(), clock.clone());
        let cfg = ClaudeAiOAuthConfig::default_with_port(45_321);
        ClaudeAiOAuthClient::new(cfg, http as Arc<dyn HttpTransport>, cm).with_clock(clock)
    }

    /// The two URLs MUST share one PKCE pair: a code obtained from the manual
    /// page is exchanged with the verifier/state minted alongside the loopback
    /// URL. Calling the single-URL builder twice would mint two pairs and the
    /// exchange could never succeed.
    #[test]
    fn the_authorize_pair_shares_one_pkce_and_differs_only_by_redirect() {
        let client = client_with(MockHttp::new(vec![]), 0);
        let AuthorizeUrlPair {
            automatic_url: auto,
            manual_url: manual,
            verifier,
            state,
        } = client.build_authorize_url_pair_with_options(
            "http://localhost:45321/callback",
            &AuthorizeOptions::default(),
        );

        assert!(!verifier.is_empty() && !state.is_empty());
        let st = format!("state={state}");
        assert!(auto.contains(&st), "auto missing state: {auto}");
        assert!(manual.contains(&st), "manual missing state: {manual}");

        assert!(auto.contains("redirect_uri=http%3A%2F%2Flocalhost%3A45321%2Fcallback"));
        assert!(
            manual.contains("oauth%2Fcode%2Fcallback"),
            "manual must use MANUAL_REDIRECT_URL: {manual}"
        );
        assert_ne!(auto, manual);

        let strip = |u: &str| {
            u.split('&')
                .filter(|p| !p.starts_with("redirect_uri="))
                .collect::<Vec<_>>()
                .join("&")
        };
        assert_eq!(
            strip(&auto),
            strip(&manual),
            "the pair must differ ONLY in redirect_uri"
        );
    }

    #[test]
    fn authorize_url_matches_current_public_oauth_contract() {
        let http = MockHttp::new(vec![]);
        let client = client_with(http, 0);
        let (url, verifier, state) = client.build_authorize_url_with_options(
            "https://platform.claude.com/oauth/code/callback",
            &AuthorizeOptions {
                scopes: Some(vec!["user:inference".into()]),
                org_uuid: Some("org/one".into()),
                login_hint: Some("a+b@example.com".into()),
                login_method: Some("sso".into()),
            },
        );

        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?code=true&"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope=user%3Ainference"));
        // `URLSearchParams` serialises as application/x-www-form-urlencoded, so
        // the space between scopes is `+`, NOT `%20`. An authorization server
        // that compares the scope string byte-for-byte sees a different value
        // between the two encodings, and the existing assertion above passes
        // under either — so pin the separator explicitly.
        assert!(!url.contains("%20"), "spaces must encode as `+`: {url}");
        assert!(url.contains("orgUUID=org%2Fone"));
        assert!(url.contains("login_hint=a%2Bb%40example.com"));
        assert!(url.contains("login_method=sso"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!verifier.is_empty());
        assert!(!state.is_empty());
    }

    #[tokio::test]
    async fn exchange_code_posts_json_and_parses_tokens() {
        let body = r#"{
            "access_token": "acc-1",
            "refresh_token": "ref-1",
            "expires_in": 3600,
            "scope": "read:user write:messages",
            "account": { "uuid": "acc-uuid", "email_address": "u@example.com" },
            "organization": { "uuid": "org-uuid" }
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
            .exchange_code("the-code", "the-verifier", "the-state")
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
        assert_eq!(tokens.scopes, vec!["read:user", "write:messages"]);
        assert_eq!(
            tokens.account.as_ref().unwrap().email_address,
            "u@example.com"
        );
        assert_eq!(tokens.organization.as_ref().unwrap().uuid, "org-uuid");

        // Assert the wire shape: POST, JSON content-type, authorization_code grant.
        let req = http.last_request().expect("a request was made");
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json"));
        let sent: serde_json::Value =
            serde_json::from_str(req.body.as_deref().unwrap()).expect("body is json");
        assert_eq!(sent["grant_type"], "authorization_code");
        assert_eq!(sent["code"], "the-code");
        assert_eq!(sent["code_verifier"], "the-verifier");
        assert_eq!(sent["state"], "the-state");
        assert_eq!(sent["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(sent["redirect_uri"], "http://localhost:45321/callback");
        assert!(sent.get("expires_in").is_none());
        assert_eq!(http.call_count(), 1);
    }

    #[tokio::test]
    async fn setup_token_exchange_requests_one_year_lifetime() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: r#"{"access_token":"a","expires_in":31536000}"#.into(),
            },
        )]);
        let client = client_with(http.clone(), 0);
        client
            .exchange_code_with_options(
                "c",
                "v",
                "s",
                "https://platform.claude.com/oauth/code/callback",
                Some(31_536_000),
            )
            .await
            .expect("exchange");
        let req = http.last_request().expect("request");
        let body: serde_json::Value =
            serde_json::from_str(req.body.as_deref().expect("body")).expect("json");
        assert_eq!(body["expires_in"], 31_536_000);
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
            .exchange_code("bad", "v", "s")
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
    async fn exchange_code_without_scope_inherits_config_scopes() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: r#"{"access_token":"a","expires_in":10}"#.into(),
            },
        )]);
        let client = client_with(http, 0);
        let tokens = client.exchange_code("c", "v", "s").await.expect("ok");
        assert!(tokens.refresh_token.is_none());
        assert_eq!(
            tokens.scopes,
            crate::oauth::anthropic::config::CLAUDE_CODE_OAUTH_SCOPES
                .iter()
                .map(|scope| (*scope).to_string())
                .collect::<Vec<_>>()
        );
        assert!(tokens.account.is_none());
    }
}
