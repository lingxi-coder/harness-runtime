//! Host [`AuthHandle`] for Anthropic OAuth login.
//!
//! Drives the full interactive PKCE Authorization-Code flow:
//!   1. SDK authorization URL construction
//!   2. open the browser (via an injectable opener — no-op in tests)
//!   3. [`crate::auth::anthropic::callback::CallbackListener::accept`] on the loopback listener
//!   4. SDK authorization-code exchange
//!   5. resolve `email` + `org_id` from the exchange response (`account` /
//!      `organization`) or, failing that, the profile endpoint — there is no
//!      JWT id-token to decode for this provider
//!   6. persist via [`secret::CredentialManager::store_oauth_tokens`]
//!   7. return [`LoginInfo`]
//!
//! The whole flow runs under a five-minute deadline (the trait contract);
//! exceeding it yields [`AuthError::Timeout`].

use crate::auth::anthropic::callback::{CallbackError, CallbackListener};
use crate::auth::anthropic::login::{prepare_exchanged_tokens, ExchangedTokens, OAuthError};
use async_trait::async_trait;
use lingxi_core::host::Clock;
use lingxi_core::host::{AuthError, AuthHandle, LoginInfo};
use lingxi_llm_client::auth::oauth::anthropic::{
    self as sdk, AuthorizeOptions, ClaudeAiOAuthConfig,
};
use lingxi_llm_client::auth::oauth::anthropic::{
    CLAUDE_CODE_INFERENCE_SCOPE, LONG_LIVED_OAUTH_TOKEN_TTL_SECONDS,
};
use lingxi_llm_client::transport::Transport;
use secret::CredentialManager;
use std::sync::Arc;
use std::time::Duration;

/// Login-flow deadline (trait contract: §login docs).
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Injectable browser opener. Production shells the platform "open" command;
/// tests pass a no-op (optionally recording the URL).
pub type BrowserOpener = Arc<dyn Fn(&str) -> Result<(), AuthError> + Send + Sync>;

/// Host callback invoked with the MANUAL authorize URL before the browser
/// opens (oracle `startOAuthFlow(async (url) => ...)`): the CLI prints the
/// "If the browser didn't open, visit:" fallback trio from it.
pub type UrlSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Host IO seams for the interactive code flow (oracle `startOAuthFlow`
/// callback + `handleManualAuthCodeInput`). Default = fully headless: no URL
/// surfaced, no manual entry (the pre-existing programmatic behaviour).
#[derive(Default)]
pub struct CodeFlowIo {
    /// Receives the MANUAL authorize URL before the browser opens.
    pub on_url: Option<UrlSink>,
    /// Manual `(code, state)` pairs pasted by the user (`code#state`). Raced
    /// against the loopback listener; a closed channel (stdin EOF) simply
    /// leaves the listener waiting.
    pub manual_rx: Option<tokio::sync::mpsc::Receiver<(String, String)>>,
}

/// Host login coordinator backed directly by SDK protocol functions.
pub struct OAuthHandle {
    config: ClaudeAiOAuthConfig,
    http: Arc<dyn Transport>,
    credentials: Arc<CredentialManager>,
    clock: Arc<dyn Clock>,
    browser_open: BrowserOpener,
}

/// Options shared by the top-level `auth login` flow and the interactive
/// `/login` default.
/// Neither `Clone` nor a derived `Debug`: the manual-code channel is a
/// single-consumer `oneshot::Receiver` (cloning it would be meaningless) and
/// the URL callback is a closure. The only construction site builds it once
/// and moves it, so neither derive was load-bearing.
#[derive(Default)]
pub struct OAuthLoginOptions {
    /// Pre-populate the account email in the provider UI.
    pub login_hint: Option<String>,
    /// Force SSO in the provider UI.
    pub sso: bool,
    /// Managed organization UUID forwarded to the provider.
    pub org_uuid: Option<String>,
}

impl std::fmt::Debug for OAuthLoginOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthLoginOptions")
            .field("login_hint", &self.login_hint)
            .field("sso", &self.sso)
            .field("org_uuid", &self.org_uuid)
            .finish()
    }
}

impl OAuthHandle {
    /// Construct a handle that opens a real browser for the login redirect.
    #[must_use]
    pub fn new(
        config: ClaudeAiOAuthConfig,
        http: Arc<dyn Transport>,
        credentials: Arc<CredentialManager>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            http,
            credentials,
            clock,
            browser_open: Arc::new(real_browser_open),
        }
    }

    /// Override the browser opener (tests inject a no-op so the flow runs
    /// headless).
    #[must_use]
    pub fn with_browser_opener(mut self, opener: BrowserOpener) -> Self {
        self.browser_open = opener;
        self
    }

    /// Build an authorization URL for a native host callback.
    ///
    /// Mobile hosts use `ASWebAuthenticationSession` instead of the loopback
    /// listener used by the CLI. The verifier and state are returned to the
    /// caller and must remain private until [`Self::complete_mobile_browser_login`]
    /// receives the callback.
    #[must_use]
    pub fn begin_mobile_browser_login(&self, redirect_uri: &str) -> (String, String, String) {
        sdk::build_authorize_url(&self.config, redirect_uri, &AuthorizeOptions::default())
    }

    /// Complete a native-host callback and persist the Anthropic session.
    ///
    /// The callback state is checked by the mobile host before this method is
    /// called and is also included in the token exchange body, preserving the
    /// provider's CSRF contract without exposing tokens to Swift/Kotlin.
    pub async fn complete_mobile_browser_login(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
    ) -> Result<LoginInfo, AuthError> {
        let tokens = self
            .exchange_tokens(code, verifier, state, redirect_uri, None)
            .await?;

        let (email, org_id) = match (&tokens.account, &tokens.organization) {
            (Some(acc), Some(org)) if !acc.email_address.is_empty() => {
                (acc.email_address.clone(), org.uuid.clone())
            }
            _ => {
                self.fetch_profile(tokens.access_token.expose_secret())
                    .await?
            }
        };

        let refresh = tokens
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        self.credentials
            .store_oauth_tokens(
                tokens.access_token.expose_secret(),
                refresh.as_deref(),
                tokens.expires_at,
                tokens.scopes.clone(),
                &email,
                &org_id,
            )
            .await
            .map_err(|e| AuthError::ServerError(format!("persist: {e}")))?;

        Ok(LoginInfo { email, org_id })
    }

    /// Run the authorization-code flow, with the manual-entry fallback wired
    /// through [`CodeFlowIo`].
    ///
    /// The loopback callback and a manually pasted code RACE: whichever
    /// arrives first wins, and the token exchange uses THAT path's
    /// redirect_uri. Exchanging a pasted code against the loopback redirect
    /// (or vice versa) is rejected by the server, so the two must not be
    /// crossed.
    async fn run_code_flow(
        &self,
        authorize: AuthorizeOptions,
        expires_in: Option<u64>,
        io: CodeFlowIo,
    ) -> Result<ExchangedTokens, AuthError> {
        // (1+) Bind the loopback listener first so the redirect target exists
        // before the browser opens. Port 0 → OS-assigned; read it back and bake
        // the real port into the redirect_uri so authorize + exchange agree.
        let listener = CallbackListener::bind(callback_port(&self.config.redirect_uri))
            .await
            .map_err(callback_to_auth_err)?;
        let redirect_uri = format!("http://localhost:{}/callback", listener.port());

        let urls = sdk::build_authorize_url_pair(&self.config, &redirect_uri, &authorize);

        // (1b) Surface the MANUAL URL variant to the host FIRST (oracle
        // `await e(i)` before `await Cc(s)`), so the fallback instructions are
        // visible even when the browser spawn below fails.
        if let Some(sink) = &io.on_url {
            sink(&urls.manual_url);
        }

        // (2) Open the browser on the AUTOMATIC variant (no-op under test).
        // Best-effort: the oracle's opener (`Cc`) reports failure as a value
        // instead of throwing, so a spawn error must NOT abort the flow — the
        // surfaced URL + manual `code#state` paste is the recovery path.
        if let Err(error) = (self.browser_open)(&urls.automatic_url) {
            tracing::warn!(
                %error,
                "could not open browser for OAuth login; complete the flow via the printed URL"
            );
        }

        // (3) Await the redirect, racing any manual `code#state` paste. The
        // socket was already bound before the browser was opened, so the kernel
        // backlog safely holds an immediate callback. Keeping the accept future
        // in this task also means a caller timeout cancels and closes the
        // listener instead of leaking a detached callback task. A manual win
        // drops (closes) the listener — oracle `handleManualAuthCodeInput`
        // closes the `authCodeListener`.
        let accept = listener.accept(&urls.state);
        tokio::pin!(accept);
        let mut manual_rx = io.manual_rx;
        let (code, via_manual) = loop {
            if let Some(rx) = manual_rx.as_mut() {
                tokio::select! {
                    res = accept.as_mut() => {
                        break (res.map_err(callback_to_auth_err)?.code, false);
                    }
                    pasted = rx.recv() => match pasted {
                        // Oracle `handleManualAuthCodeInput` consumes only the
                        // CODE; the exchange runs with the flow's own state and
                        // the manual redirect_uri (`useManualRedirect`), so the
                        // pasted state is syntax-validated by the CLI but not
                        // re-checked here (the token endpoint enforces it).
                        Some((code, _state)) => break (code, true),
                        // Manual channel closed (stdin EOF) — keep waiting on
                        // the loopback listener alone.
                        None => manual_rx = None,
                    },
                }
            } else {
                break (
                    accept.as_mut().await.map_err(callback_to_auth_err)?.code,
                    false,
                );
            }
        };

        // (4) Exchange the code for tokens. A manually-pasted code was minted
        // against the hosted code page, so the exchange must present THAT
        // redirect_uri (oracle `useManualRedirect: !automatic`).
        let exchange_redirect = if via_manual {
            self.config.manual_redirect_uri.clone()
        } else {
            redirect_uri
        };
        self.exchange_tokens(
            &code,
            &urls.verifier,
            &urls.state,
            &exchange_redirect,
            expires_in,
        )
        .await
    }

    /// Exchange using SDK wire protocol, then apply host secret and clock policy.
    async fn exchange_tokens(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
        expires_in: Option<u64>,
    ) -> Result<ExchangedTokens, AuthError> {
        let response = sdk::exchange_code(
            self.http.as_ref(),
            &self.config,
            code,
            verifier,
            state,
            redirect_uri,
            expires_in,
        )
        .await
        .map_err(|e| {
            AuthError::ServerError(OAuthError::TokenExchange(e.to_string()).to_string())
        })?;
        Ok(prepare_exchanged_tokens(
            response,
            &self.config,
            self.clock.as_ref(),
        ))
    }

    /// Run the normal login flow and persist the resulting credential.
    pub async fn login_with_options(
        &self,
        options: OAuthLoginOptions,
    ) -> Result<LoginInfo, AuthError> {
        self.login_with_options_and_io(options, CodeFlowIo::default())
            .await
    }

    /// [`Self::login_with_options`] with host IO seams: `io.on_url` receives
    /// the MANUAL authorize URL before the browser opens, and `io.manual_rx`
    /// feeds pasted `code#state` pairs that race the loopback callback (the
    /// `claude auth login` browser-failure fallback).
    pub async fn login_with_options_and_io(
        &self,
        options: OAuthLoginOptions,
        io: CodeFlowIo,
    ) -> Result<LoginInfo, AuthError> {
        let authorize = AuthorizeOptions {
            org_uuid: options.org_uuid,
            login_hint: options.login_hint,
            login_method: options.sso.then(|| "sso".to_string()),
            scopes: None,
        };
        let tokens = self.run_code_flow(authorize, None, io).await?;

        // (5) Resolve email + org. Prefer the exchange response; otherwise fetch
        //     the profile endpoint with the bearer token.
        let (email, org_id) = match (&tokens.account, &tokens.organization) {
            (Some(acc), Some(org)) if !acc.email_address.is_empty() => {
                (acc.email_address.clone(), org.uuid.clone())
            }
            _ => {
                self.fetch_profile(tokens.access_token.expose_secret())
                    .await?
            }
        };

        // (6) Persist tokens + identity.
        let refresh = tokens
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        self.credentials
            .store_oauth_tokens(
                tokens.access_token.expose_secret(),
                refresh.as_deref(),
                tokens.expires_at,
                tokens.scopes.clone(),
                &email,
                &org_id,
            )
            .await
            .map_err(|e| AuthError::ServerError(format!("persist: {e}")))?;

        // (6b) Resolve the subscription tier (claude-code `getOauthAccountInfo`,
        // written at login from the profile + roles endpoints), then BOTH
        // persist it into the stored credential (claude-code keeps
        // `subscriptionType`/`rateLimitTier` INSIDE `claudeAiOauth`, so a fresh
        // process has correct enterprise/tier state on its FIRST request) and
        // publish it to the process-global `lingxi_core::host::subscription` cache for
        // subscription-gated prompt logic (e.g. the `AgentTool` pro-plan gate).
        // Best-effort + scope-gated (`hasProfileScope`): a token without
        // `user:profile`, or any fetch failure, leaves both unchanged.
        let transport = self.http.clone();
        if let Some(snapshot) = crate::auth::anthropic::subscription::resolve_subscription_snapshot(
            tokens.access_token.expose_secret(),
            &tokens.scopes,
            &transport,
        )
        .await
        {
            if let Err(error) = self
                .credentials
                .update_oauth_subscription(
                    snapshot.subscription_type.as_deref(),
                    snapshot.rate_limit_tier.as_deref(),
                )
                .await
            {
                tracing::warn!(%error, "could not persist subscription tier into the credential store");
            }
            lingxi_core::host::subscription::set_current_subscription(Some(snapshot));
        }

        // (7) Return the resolved identity.
        Ok(LoginInfo { email, org_id })
    }

    /// Mint a one-year, inference-only token without persisting it locally.
    /// The caller must print/store it exactly once.
    pub async fn mint_long_lived_token(
        &self,
        org_uuid: Option<String>,
    ) -> Result<lingxi_core::types::Secret<String>, AuthError> {
        let tokens = self
            .run_code_flow(
                AuthorizeOptions {
                    scopes: Some(vec![CLAUDE_CODE_INFERENCE_SCOPE.to_string()]),
                    org_uuid,
                    login_hint: None,
                    login_method: None,
                },
                Some(LONG_LIVED_OAUTH_TOKEN_TTL_SECONDS),
                CodeFlowIo::default(),
            )
            .await?;
        Ok(tokens.access_token)
    }

    /// GET the profile endpoint with `Authorization: Bearer <access>`.
    async fn fetch_profile(&self, access_token: &str) -> Result<(String, String), AuthError> {
        lingxi_llm_client::auth::oauth::anthropic::fetch_login_identity(
            self.http.as_ref(),
            &self.config.profile_endpoint,
            access_token,
        )
        .await
        .map_err(AuthError::ServerError)
    }
}

#[async_trait]
impl AuthHandle for OAuthHandle {
    async fn login(&self) -> Result<LoginInfo, AuthError> {
        match tokio::time::timeout(
            LOGIN_TIMEOUT,
            self.login_with_options(OAuthLoginOptions::default()),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(AuthError::Timeout),
        }
    }

    async fn logout(&self) -> Result<(), AuthError> {
        // Idempotent — deleting absent entries is not an error.
        self.credentials
            .delete_oauth_tokens()
            .await
            .map_err(|e| AuthError::ServerError(format!("logout: {e}")))
    }

    async fn current_user(&self) -> Option<LoginInfo> {
        match self.credentials.get_oauth_tokens().await {
            Ok(Some(t)) => Some(LoginInfo {
                email: t.email,
                org_id: t.org_id,
            }),
            _ => None,
        }
    }
}

/// Parse the loopback port out of a `http://127.0.0.1:{port}/callback`-style
/// redirect URI. Falls back to `0` (OS-assigned) when the URI is `:0` or
/// unparseable.
fn callback_port(redirect_uri: &str) -> u16 {
    redirect_uri
        .rsplit_once(':')
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0)
}

/// Map a callback-layer failure onto the auth surface. A state mismatch /
/// cancellation reads as a user cancellation; bind/parse failures are server
/// errors.
fn callback_to_auth_err(e: CallbackError) -> AuthError {
    match e {
        CallbackError::StateMismatch => AuthError::Cancelled,
        CallbackError::Bind(m) => AuthError::ServerError(format!("loopback bind: {m}")),
        CallbackError::InvalidRequest(m) => AuthError::ServerError(format!("callback: {m}")),
        // The provider redirected with `error=` — most often the user pressed
        // Deny, which is a cancellation, not a server fault.
        CallbackError::Provider(detail) => AuthError::ServerError(format!("denied: {detail}")),
    }
}

/// Open `url` in the user's default browser via the platform command. Best
/// effort — a spawn failure surfaces as a server error, which `run_code_flow`
/// logs and IGNORES (oracle `Cc` reports failure as a value): the surfaced
/// manual URL + `code#state` paste is the recovery path.
fn real_browser_open(url: &str) -> Result<(), AuthError> {
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);

    std::process::Command::new(cmd)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| AuthError::ServerError(format!("could not open browser: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::anthropic::testsupport::{
        mem_credential_manager, Canned, MemStorage, MockHttp, TestClock,
    };
    use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
    use lingxi_llm_client::transport::Transport;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[allow(dead_code)]
    fn _oauth_handle_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OAuthHandle>();
    }

    #[test]
    fn auth_handle_trait_object_compiles() {
        fn _accept(_h: &Arc<dyn AuthHandle>) {}
    }

    /// Build a handle whose token endpoint returns `token_body`, over an
    /// in-memory keychain. Returns the handle, the storage (for assertions),
    /// a flag set when the browser opener fired, and the transport (for
    /// request-BODY assertions — the exchange carries `redirect_uri` in the
    /// body, never in the URL).
    fn handle_with_token_body(
        token_body: &str,
    ) -> (OAuthHandle, Arc<MemStorage>, Arc<AtomicBool>, Arc<MockHttp>) {
        handle_with_bodies(
            token_body,
            r#"{"account":{"email":"profile@example.com"},"organization":{"uuid":"org-from-profile"}}"#,
        )
    }

    /// [`handle_with_token_body`] with an explicit profile-endpoint body (used
    /// by the subscription-persistence test, which needs an org tier).
    fn handle_with_bodies(
        token_body: &str,
        profile_body: &str,
    ) -> (OAuthHandle, Arc<MemStorage>, Arc<AtomicBool>, Arc<MockHttp>) {
        let http = MockHttp::new(vec![
            (
                "oauth/token",
                Canned {
                    status: 200,
                    body: token_body.into(),
                },
            ),
            // Profile endpoint (identity fallback when the token body omits
            // account/org, and the subscription-tier resolution).
            (
                "/api/oauth/profile",
                Canned {
                    status: 200,
                    body: profile_body.into(),
                },
            ),
        ]);
        let clock = TestClock::new(1_000);
        let storage = MemStorage::new();
        let cm = mem_credential_manager(storage.clone(), clock.clone());
        let cfg = ClaudeAiOAuthConfig::default_with_port(0);
        let handle = OAuthHandle::new(cfg, http.clone() as Arc<dyn Transport>, cm, clock);

        let was_opened = Arc::new(AtomicBool::new(false));
        let browser_flag = was_opened.clone();
        // The "browser opener" instead drives the loopback callback: it parses
        // the redirect_uri + state out of the authorize URL and POSTs the GET
        // to the listener, completing the flow headlessly.
        let opener: BrowserOpener = Arc::new(move |url: &str| {
            browser_flag.store(true, Ordering::SeqCst);
            let url = url.to_string();
            // Spawn so the opener returns immediately while the listener accepts.
            tokio::spawn(async move {
                let (port, state) = parse_authorize_url(&url);
                // Small spin until the listener is ready.
                for _ in 0..50 {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
                        let req = format!(
                            "GET /callback?code=AUTHCODE&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                        );
                        let _ = s.write_all(req.as_bytes()).await;
                        let mut buf = Vec::new();
                        let _ = s.read_to_end(&mut buf).await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            Ok(())
        });

        (
            handle.with_browser_opener(opener),
            storage,
            was_opened,
            http,
        )
    }

    #[tokio::test]
    async fn exchange_errors_keep_host_prefix_and_invalid_code_message() {
        for (status, expected) in [
            (
                401,
                "token exchange failed: Authentication failed: Invalid authorization code",
            ),
            (500, "token exchange failed:"),
        ] {
            let http = MockHttp::new(vec![(
                "oauth/token",
                Canned {
                    status,
                    body: r#"{"error":"invalid_grant"}"#.into(),
                },
            )]);
            let clock = TestClock::new(1_000);
            let credentials = mem_credential_manager(MemStorage::new(), clock.clone());
            let config = ClaudeAiOAuthConfig::default_with_port(0);
            let handle = OAuthHandle::new(config, http, credentials, clock);
            let error = handle
                .exchange_tokens(
                    "bad",
                    "verifier",
                    "state",
                    "http://localhost:0/callback",
                    None,
                )
                .await
                .expect_err("token endpoint must reject the exchange");
            let AuthError::ServerError(message) = error else {
                panic!("expected server error");
            };
            assert!(message.starts_with(expected), "{message}");
        }
    }

    /// `redirect_uri` off the `oauth/token` POST body. It travels ONLY in the
    /// body (the URL is the fixed `token_endpoint`), so the transport's URL
    /// routing cannot observe it — assertions on the exchange must come here.
    fn exchange_redirect_uri(http: &MockHttp) -> String {
        let reqs = http.sdk_requests.lock().unwrap();
        let exchange = reqs
            .iter()
            .find(|r| r.url.contains("oauth/token"))
            .expect("token exchange was issued");
        let body: serde_json::Value =
            serde_json::from_slice(&exchange.body).expect("exchange body is JSON");
        body["redirect_uri"]
            .as_str()
            .expect("exchange body carries redirect_uri")
            .to_string()
    }

    fn parse_authorize_url(url: &str) -> (u16, String) {
        // redirect_uri is percent-encoded; find it and pull the port. state is
        // a plain query param.
        let query = url.split_once('?').map_or("", |(_, q)| q);
        let mut redirect = String::new();
        let mut state = String::new();
        for kv in query.split('&') {
            if let Some(v) = kv.strip_prefix("redirect_uri=") {
                redirect = urlencoding::decode(v)
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
            } else if let Some(v) = kv.strip_prefix("state=") {
                state = urlencoding::decode(v)
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default();
            }
        }
        let port = redirect
            .rsplit_once(':')
            .and_then(|(_, rest)| rest.split('/').next())
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(0);
        (port, state)
    }

    #[tokio::test]
    async fn login_uses_account_org_from_token_response() {
        // Binds the fixed loopback port 45321 — same machine-global resource
        // contention as the OpenAI ports, and these three raced each other.
        let _g = crate::auth::openai::testsupport::port_guard().await;
        let body = r#"{
            "access_token":"acc","refresh_token":"ref","expires_in":3600,
            "scope":"read:user",
            "account":{"uuid":"acc-uuid","email_address":"token@example.com"},
            "organization":{"uuid":"org-token"}
        }"#;
        let (handle, storage, opened, _http) = handle_with_token_body(body);
        let info = handle.login().await.expect("login ok");
        assert_eq!(info.email, "token@example.com");
        assert_eq!(info.org_id, "org-token");
        assert!(opened.load(Ordering::SeqCst), "browser opener fired");
        // 3 OAuth entries persisted (access + refresh + meta).
        assert_eq!(storage.count("lingxi"), 3);

        // current_user reads them back without a network call.
        let cu = handle.current_user().await.expect("current user");
        assert_eq!(cu.email, "token@example.com");
        assert_eq!(cu.org_id, "org-token");
    }

    #[tokio::test]
    async fn login_falls_back_to_profile_endpoint() {
        // Binds the fixed loopback port 45321 — same machine-global resource
        // contention as the OpenAI ports, and these three raced each other.
        let _g = crate::auth::openai::testsupport::port_guard().await;
        // Token body omits account/organization → profile GET is used.
        let body = r#"{"access_token":"acc","refresh_token":"ref","expires_in":3600}"#;
        let (handle, _storage, _opened, _http) = handle_with_token_body(body);
        let info = handle.login().await.expect("login ok");
        assert_eq!(info.email, "profile@example.com");
        assert_eq!(info.org_id, "org-from-profile");
    }

    #[tokio::test]
    async fn logout_clears_and_current_user_is_none() {
        // Binds the fixed loopback port 45321 — same machine-global resource
        // contention as the OpenAI ports, and these three raced each other.
        let _g = crate::auth::openai::testsupport::port_guard().await;
        let body = r#"{"access_token":"acc","refresh_token":"ref","expires_in":3600,
            "account":{"uuid":"u","email_address":"e@x"},"organization":{"uuid":"o"}}"#;
        let (handle, storage, _, _http) = handle_with_token_body(body);
        handle.login().await.expect("login ok");
        assert_eq!(storage.count("lingxi"), 3);

        handle.logout().await.expect("logout ok");
        assert_eq!(storage.count("lingxi"), 0);
        assert!(handle.current_user().await.is_none());

        // Idempotent.
        handle.logout().await.expect("second logout ok");
    }

    #[tokio::test]
    async fn current_user_none_when_not_logged_in() {
        let body = r#"{"access_token":"acc","expires_in":1}"#;
        let (handle, _storage, _, _http) = handle_with_token_body(body);
        assert!(handle.current_user().await.is_none());
    }

    /// M8: a failing browser opener no longer aborts the flow, the host URL
    /// sink receives the MANUAL authorize URL, and a pasted `code#state` pair
    /// completes the login — with the token exchange presenting the MANUAL
    /// redirect_uri (oracle `useManualRedirect`).
    #[tokio::test]
    async fn manual_code_entry_completes_login_when_browser_fails() {
        // Binds the fixed loopback port 45321 — same machine-global resource
        // contention as the OpenAI ports, and these tests raced each other.
        let _g = crate::auth::openai::testsupport::port_guard().await;
        let body = r#"{
            "access_token":"acc","refresh_token":"ref","expires_in":3600,
            "scope":"read:user",
            "account":{"uuid":"acc-uuid","email_address":"token@example.com"},
            "organization":{"uuid":"org-token"}
        }"#;
        let (handle, _storage, opened, http) = handle_with_token_body(body);
        // Failing opener: never drives the loopback callback.
        let handle = handle.with_browser_opener(Arc::new(|_url: &str| {
            Err(AuthError::ServerError(
                "could not open browser: boom".into(),
            ))
        }));

        let seen_url = Arc::new(std::sync::Mutex::new(String::new()));
        let sink_url = seen_url.clone();
        let on_url: UrlSink = Arc::new(move |url: &str| {
            *sink_url.lock().unwrap() = url.to_string();
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
        tx.send(("PASTEDCODE".into(), "pasted-state".into()))
            .await
            .expect("queue manual code");

        let info = handle
            .login_with_options_and_io(
                OAuthLoginOptions::default(),
                CodeFlowIo {
                    on_url: Some(on_url),
                    manual_rx: Some(rx),
                },
            )
            .await
            .expect("login completes via manual entry despite browser failure");
        assert_eq!(info.email, "token@example.com");
        assert!(!opened.load(Ordering::SeqCst), "flag opener replaced");

        // The sink got the MANUAL URL variant (hosted code page redirect).
        let url = seen_url.lock().unwrap().clone();
        assert!(
            url.contains(
                "redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"
            ),
            "sink got the manual variant, was: {url}"
        );

        // …and so did the EXCHANGE. The pasted code was minted against the
        // hosted code page, so presenting the loopback redirect here is an
        // `invalid_grant` — oracle `iJi` @228909300:
        // `redirect_uri: o ? Ds().MANUAL_REDIRECT_URL : \`http://localhost:${n}/callback\``
        // with `useManualRedirect: !l` (@234950372, `l = hasPendingResponse()`).
        assert_eq!(
            exchange_redirect_uri(&http),
            "https://platform.claude.com/oauth/code/callback",
            "manual paste must exchange against MANUAL_REDIRECT_URL"
        );
    }

    /// Companion to the manual case: when the loopback listener wins the race
    /// (`hasPendingResponse()` true ⇒ `useManualRedirect: false`), the exchange
    /// must present the LOOPBACK redirect the code was actually minted against.
    /// Together the two pin both arms of the `exchange_redirect` selection.
    #[tokio::test]
    async fn loopback_callback_exchanges_against_the_loopback_redirect() {
        // Binds the fixed loopback port 45321 — same machine-global resource
        // contention as the OpenAI ports, and these tests raced each other.
        let _g = crate::auth::openai::testsupport::port_guard().await;
        let body = r#"{
            "access_token":"acc","refresh_token":"ref","expires_in":3600,
            "scope":"read:user",
            "account":{"uuid":"acc-uuid","email_address":"token@example.com"},
            "organization":{"uuid":"org-token"}
        }"#;
        let (handle, _storage, opened, http) = handle_with_token_body(body);
        handle.login().await.expect("login ok");
        assert!(
            opened.load(Ordering::SeqCst),
            "loopback opener drove the flow"
        );

        let redirect = exchange_redirect_uri(&http);
        let port = redirect
            .strip_prefix("http://localhost:")
            .and_then(|rest| rest.strip_suffix("/callback"))
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("expected a loopback redirect, was: {redirect}"));
        assert!(
            port > 0,
            "redirect must carry the bound port, was: {redirect}"
        );
    }

    /// M13: a login whose token carries `user:profile` resolves the profile
    /// tier and PERSISTS `subscription_type`/`rate_limit_tier` into the stored
    /// credential (claude-code keeps them inside `claudeAiOauth`), so a fresh
    /// process seeds correct enterprise/tier state before any network.
    #[tokio::test]
    async fn login_persists_subscription_tier_from_profile() {
        // Binds the fixed loopback port 45321 — same machine-global resource
        // contention as the OpenAI ports, and these tests raced each other.
        let _g = crate::auth::openai::testsupport::port_guard().await;
        // This login WRITES the process-global subscription cache; serialize
        // with the other global-cache tests.
        let _s = crate::auth::anthropic::testsupport::SUBSCRIPTION_CACHE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let token_body = r#"{
            "access_token":"acc","refresh_token":"ref","expires_in":3600,
            "scope":"org:create_api_key user:profile user:inference",
            "account":{"uuid":"acc-uuid","email_address":"token@example.com"},
            "organization":{"uuid":"org-token"}
        }"#;
        let profile_body = r#"{"organization":{"organization_type":"claude_max","uuid":"org-token",
            "rate_limit_tier":"default_claude_max_20x"}}"#;
        let (handle, storage, _, _http) = handle_with_bodies(token_body, profile_body);
        handle.login().await.expect("login ok");

        let clock = TestClock::new(1_000);
        let cm = mem_credential_manager(storage.clone(), clock);
        let tokens = cm
            .get_oauth_tokens()
            .await
            .expect("get")
            .expect("persisted");
        assert_eq!(tokens.subscription_type.as_deref(), Some("max"));
        assert_eq!(
            tokens.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
        lingxi_core::host::subscription::set_current_subscription(None);
    }

    /// M8: with NO manual channel and a dead opener the flow still waits on
    /// the listener (regression: it used to abort with the opener error). The
    /// caller timeout is what ends it.
    #[tokio::test]
    async fn browser_failure_alone_keeps_waiting_not_error() {
        let _g = crate::auth::openai::testsupport::port_guard().await;
        let body = r#"{"access_token":"acc","expires_in":3600}"#;
        let (handle, _storage, _, _http) = handle_with_token_body(body);
        let handle = handle.with_browser_opener(Arc::new(|_url: &str| {
            Err(AuthError::ServerError("could not open browser".into()))
        }));
        let out = tokio::time::timeout(
            Duration::from_millis(200),
            handle.login_with_options(OAuthLoginOptions::default()),
        )
        .await;
        assert!(out.is_err(), "flow keeps waiting instead of aborting");
    }
}
