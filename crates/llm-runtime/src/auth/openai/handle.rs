//! [`OpenAiOAuthHandle`] — browser-PKCE and device-code login for the
//! `OpenAI` / `ChatGPT` OAuth flow.
//!
//! Drives either:
//!   A. PKCE Authorization-Code flow (`login`):
//!      1. [`CallbackListener::bind`] on ports 1455/1457
//!      2. SDK config builds the authorization URL.
//!      3. Open browser (injectable — no-op in tests)
//!      4. [`CallbackListener::accept`] — validate state
//!      5. SDK exchanges the authorization code.
//!      6. SDK parses `id_token` for `account_id` / fedramp.
//!      7. SDK mints an `sk-...` key.
//!      8. Persist via [`secret::CredentialManager::store_openai_oauth_tokens`]
//!      9. Persist the minted API key via
//!         [`secret::CredentialManager::set_provider_key`] (under id `"chatgpt"`)
//!      10. Return [`OpenAiLoginInfo`]
//!   B. Device-code flow (`login_device_code`):
//!      [`device_code::run_device_code_login`] → same persist steps.
//!
//! **Trait note:** we expose INHERENT async methods rather than implementing
//! `lingxi_core::host::AuthHandle`, because that trait's `LoginInfo` type carries
//! `email`+`org_id` (Anthropic-shaped) whereas `ChatGPT` carries `account_id`+
//! `fedramp`. The engine M8 wrapper (`ChatGptConnectDriver`) will call these
//! inherent methods directly.

use crate::auth::openai::callback::{CallbackError, CallbackListener};
use crate::auth::openai::device_code;
use crate::auth::openai::login::{exchange_error, into_login_tokens};
use lingxi_core::host::Clock;
use lingxi_llm_client::auth::oauth::openai::{self as sdk, OpenAiOAuthConfig};
use lingxi_llm_client::Transport;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// Wall-clock source for production use in the device-code path.
struct RealClock;
impl Clock for RealClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Login-flow deadline for the browser-PKCE path.
///
/// 60 s could not cover a real sign-in — email, password, and an MFA prompt —
/// and expired the flow mid-login. Codex sets no deadline at all on this path;
/// 10 minutes is a bound that still releases the fixed callback port.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// Injectable browser opener. Production shells the platform "open" command;
/// tests pass a no-op (optionally recording the URL).
pub type BrowserOpener = Arc<dyn Fn(&str) -> Result<(), OpenAiAuthError> + Send + Sync>;

/// Error surface for the `OpenAI` handle.
#[derive(Debug, Error)]
pub enum OpenAiAuthError {
    /// Network or server-side failure.
    #[error("server error: {0}")]
    ServerError(String),
    /// User cancelled (state mismatch / Ctrl-C).
    #[error("login cancelled")]
    Cancelled,
    /// The 60-second browser-flow deadline elapsed.
    #[error("login timed out")]
    Timeout,
    /// Could not open the browser.
    #[error("browser open failed: {0}")]
    BrowserOpen(String),
}

/// What we know about the signed-in `ChatGPT` account after a successful login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiLoginInfo {
    /// `chatgpt_account_id` from the `id_token` JWT (may be absent on older tokens).
    pub account_id: Option<String>,
    /// `FedRAMP` account flag from the `id_token` JWT.
    pub fedramp: bool,
    /// `email` claim from the `id_token` JWT (may be absent on older tokens).
    pub email: Option<String>,
    /// Minted `sk-...` API key from the RFC-8693 exchange.
    pub api_key: String,
}

/// Handle that drives either the browser-PKCE or device-code `OpenAI` OAuth flow
/// and persists the resulting tokens in the `secret` keychain.
pub struct OpenAiOAuthHandle {
    config: OpenAiOAuthConfig,
    http: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    credentials: Arc<secret::CredentialManager>,
    browser_open: BrowserOpener,
}

impl OpenAiOAuthHandle {
    /// Construct a handle that opens a real browser for the login redirect.
    #[must_use]
    pub fn new(
        config: OpenAiOAuthConfig,
        http: Arc<dyn Transport>,
        credentials: Arc<secret::CredentialManager>,
    ) -> Self {
        Self {
            config,
            http,
            clock: Arc::new(RealClock),
            credentials,
            browser_open: Arc::new(real_browser_open),
        }
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Override the browser opener (tests inject a no-op so the flow runs
    /// headlessly).
    #[must_use]
    pub fn with_browser_opener(mut self, opener: BrowserOpener) -> Self {
        self.browser_open = opener;
        self
    }

    /// Build an authorization URL for a native host callback.
    ///
    /// iOS supplies a custom-scheme redirect to `ASWebAuthenticationSession`,
    /// so this path deliberately does not bind the desktop loopback listener.
    #[must_use]
    pub fn begin_mobile_browser_login(&self, redirect_uri: &str) -> (String, String, String) {
        self.config.build_authorize_url_with_redirect(redirect_uri)
    }

    /// Complete a native-host callback and persist the ChatGPT session.
    pub async fn complete_mobile_browser_login(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<OpenAiLoginInfo, OpenAiAuthError> {
        let tokens = sdk::exchange_code(
            self.http.as_ref(),
            &self.config,
            code,
            verifier,
            redirect_uri,
        )
        .await
        .map_err(exchange_error)
        .map_err(|e| OpenAiAuthError::ServerError(e.to_string()))?;
        let tokens = into_login_tokens(tokens, self.clock.as_ref());

        let claims = tokens
            .id_token
            .as_deref()
            .and_then(sdk::parse_id_token)
            .unwrap_or_default();
        // Persist the OAuth session BEFORE minting the API key: the mint is
        // best-effort (see `mint_api_key_best_effort`), and failing it used to
        // throw away an already-completed sign-in.
        let refresh = tokens
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        self.credentials
            .store_openai_oauth_tokens(
                tokens.access_token.expose_secret(),
                refresh.as_deref(),
                tokens.expires_at,
                vec![],
                claims.account_id.as_deref(),
                claims.fedramp,
                claims.email.as_deref(),
            )
            .await
            .map_err(|e| OpenAiAuthError::ServerError(format!("persist tokens: {e}")))?;
        let api_key = self
            .mint_api_key_best_effort(tokens.id_token.as_deref())
            .await;

        Ok(OpenAiLoginInfo {
            account_id: claims.account_id,
            fedramp: claims.fedramp,
            email: claims.email,
            api_key,
        })
    }

    /// Mint the RFC-8693 API key and persist it under credential id
    /// `chatgpt` — best-effort, never fatal.
    ///
    /// Codex treats the mint as optional: `.ok()` on the loopback path
    /// (`codex-rs/login/src/server.rs:408-415`) and a literal `None` on the
    /// device path (`device_code_auth.rs:224`), because an account on a
    /// subscription-only ChatGPT plan cannot create API keys at all. Hard
    /// failing here discarded a sign-in that had already succeeded, taking the
    /// OAuth tokens with it.
    ///
    /// The empty string on failure is not load-bearing: mobile inference reads
    /// the bearer from `OpenAiOAuthCredentialProvider` (backed by the refresh
    /// driver), not this slot, and `current_user` already defaults it.
    async fn mint_api_key_best_effort(&self, id_token: Option<&str>) -> String {
        let Some(id_token) = id_token else {
            tracing::warn!(
                target: "lingxi::openai_oauth",
                "token response carried no id_token; skipping the API-key mint",
            );
            return String::new();
        };
        match sdk::obtain_api_key(self.http.as_ref(), &self.config, id_token).await {
            Ok(api_key) => {
                if let Err(error) = self.credentials.set_provider_key("chatgpt", &api_key).await {
                    tracing::warn!(
                        target: "lingxi::openai_oauth",
                        %error,
                        "could not persist the minted API key; the OAuth session stands",
                    );
                }
                api_key
            }
            Err(error) => {
                tracing::warn!(
                    target: "lingxi::openai_oauth",
                    %error,
                    "API-key mint failed; continuing with the OAuth session",
                );
                String::new()
            }
        }
    }

    // ── Browser-PKCE flow ─────────────────────────────────────────────────

    /// Run the full browser-PKCE Authorization-Code flow.
    ///
    /// Wraps [`run_login`] in a 60-second deadline.
    pub async fn login(&self) -> Result<OpenAiLoginInfo, OpenAiAuthError> {
        match tokio::time::timeout(LOGIN_TIMEOUT, self.run_login()).await {
            Ok(result) => result,
            Err(_elapsed) => Err(OpenAiAuthError::Timeout),
        }
    }

    /// Steps 1–10 without the outer timeout.
    async fn run_login(&self) -> Result<OpenAiLoginInfo, OpenAiAuthError> {
        // (1) Bind the loopback listener so the redirect target exists before
        // the browser opens. The fixed-port bind (1455 / 1457) mirrors codex.
        let listener = CallbackListener::bind()
            .await
            .map_err(callback_to_auth_err)?;
        let bound_port = listener.port();
        let redirect_uri = self.config.redirect_uri(bound_port);

        // (2) Build the authorize URL.
        let (url, verifier, state) = self.config.build_authorize_url_with_redirect(&redirect_uri);

        // Start listening BEFORE opening the browser.
        let accept_state = state.clone();
        let accept = tokio::spawn(async move { listener.accept(&accept_state).await });

        // (3) Open the browser (no-op under test).
        (self.browser_open)(&url)?;

        // (4) Await the redirect.
        let params = accept
            .await
            .map_err(|e| OpenAiAuthError::ServerError(format!("callback task: {e}")))?
            .map_err(callback_to_auth_err)?;

        // (5) Exchange the code for tokens.
        let tokens = sdk::exchange_code(
            self.http.as_ref(),
            &self.config,
            &params.code,
            &verifier,
            &redirect_uri,
        )
        .await
        .map_err(exchange_error)
        .map_err(|e| OpenAiAuthError::ServerError(e.to_string()))?;
        let tokens = into_login_tokens(tokens, self.clock.as_ref());

        // (6) Parse id_token for account_id / fedramp.
        let claims = tokens
            .id_token
            .as_deref()
            .and_then(sdk::parse_id_token)
            .unwrap_or_default();

        // (7) Persist OAuth tokens. This comes BEFORE the API-key mint, which
        // is best-effort — see `mint_api_key_best_effort`.
        let refresh = tokens
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        self.credentials
            .store_openai_oauth_tokens(
                tokens.access_token.expose_secret(),
                refresh.as_deref(),
                tokens.expires_at,
                vec![], // scopes not returned in exchange response body
                claims.account_id.as_deref(),
                claims.fedramp,
                claims.email.as_deref(),
            )
            .await
            .map_err(|e| OpenAiAuthError::ServerError(format!("persist tokens: {e}")))?;

        // (8) Mint an API key via RFC-8693 token exchange and persist it under
        // credential id "chatgpt", best-effort.
        let api_key = self
            .mint_api_key_best_effort(tokens.id_token.as_deref())
            .await;

        // (10) Return resolved identity.
        Ok(OpenAiLoginInfo {
            account_id: claims.account_id,
            fedramp: claims.fedramp,
            email: claims.email,
            api_key,
        })
    }

    // ── Device-code flow ──────────────────────────────────────────────────

    /// Run the device-code login flow.
    ///
    /// Calls `device_code::run_device_code_login`, parses the `id_token` claims,
    /// mints an API key, and persists everything — same storage as `login()`.
    pub async fn login_device_code(&self) -> Result<OpenAiLoginInfo, OpenAiAuthError> {
        let tokens = device_code::run_device_code_login(
            self.config.clone(),
            self.http.clone(),
            self.clock.clone(),
        )
        .await
        .map_err(|e| OpenAiAuthError::ServerError(e.to_string()))?;

        let claims = tokens
            .id_token
            .as_deref()
            .and_then(sdk::parse_id_token)
            .unwrap_or_default();

        let refresh = tokens
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        self.credentials
            .store_openai_oauth_tokens(
                tokens.access_token.expose_secret(),
                refresh.as_deref(),
                tokens.expires_at,
                vec![],
                claims.account_id.as_deref(),
                claims.fedramp,
                claims.email.as_deref(),
            )
            .await
            .map_err(|e| OpenAiAuthError::ServerError(format!("persist tokens: {e}")))?;

        let api_key = self
            .mint_api_key_best_effort(tokens.id_token.as_deref())
            .await;

        Ok(OpenAiLoginInfo {
            account_id: claims.account_id,
            fedramp: claims.fedramp,
            email: claims.email,
            api_key,
        })
    }

    // ── Session helpers ───────────────────────────────────────────────────

    /// Retrieve the currently-persisted session identity without any network
    /// call. Returns `None` when no valid session exists.
    pub async fn current_user(&self) -> Option<OpenAiLoginInfo> {
        // We read back what was persisted: account_id, fedramp, email from the
        // meta, and the api_key from the provider-key slot.
        let tokens = self
            .credentials
            .get_openai_oauth_tokens()
            .await
            .ok()
            .flatten()?;
        let api_key = self
            .credentials
            .get_provider_key("chatgpt")
            .await
            .ok()
            .flatten()
            .map(|s| s.expose_secret().clone())
            .unwrap_or_default();
        Some(OpenAiLoginInfo {
            account_id: tokens.account_id,
            fedramp: tokens.fedramp,
            email: tokens.email,
            api_key,
        })
    }

    /// Clear all persisted `OpenAI` OAuth credentials. Idempotent.
    pub async fn logout(&self) -> Result<(), OpenAiAuthError> {
        self.credentials
            .delete_openai_oauth_tokens()
            .await
            .map_err(|e| OpenAiAuthError::ServerError(format!("logout tokens: {e}")))?;
        // Also remove the minted API key. Keeping an empty placeholder in the
        // keychain made the post-logout credential state look configured.
        self.credentials
            .delete_provider_key("chatgpt")
            .await
            .map_err(|e| OpenAiAuthError::ServerError(format!("logout api key: {e}")))?;
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn callback_to_auth_err(e: CallbackError) -> OpenAiAuthError {
    match e {
        CallbackError::StateMismatch => OpenAiAuthError::Cancelled,
        CallbackError::Bind(m) => OpenAiAuthError::ServerError(format!("loopback bind: {m}")),
        CallbackError::InvalidRequest(m) => OpenAiAuthError::ServerError(format!("callback: {m}")),
        // The provider redirected with `error=` — most often the user pressed
        // Deny, which is a cancellation, not a server fault.
        CallbackError::Provider(detail) => {
            OpenAiAuthError::ServerError(format!("denied: {detail}"))
        }
    }
}

/// Open `url` in the user's default browser via the platform command.
fn real_browser_open(url: &str) -> Result<(), OpenAiAuthError> {
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
        .map_err(|e| OpenAiAuthError::BrowserOpen(e.to_string()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::openai::testsupport::{
        mem_credential_manager, port_guard, Canned, MemStorage, MockHttp, TestClock,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[allow(dead_code)]
    fn _openai_oauth_handle_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OpenAiOAuthHandle>();
    }

    /// Build a handle whose token endpoint returns `token_body`, plus an
    /// api-key exchange that returns `"sk-test-key"`.
    ///
    /// Returns `(handle, storage, browser_opened_flag)`.
    fn handle_with_token_body(
        token_body: &str,
    ) -> (OpenAiOAuthHandle, Arc<MemStorage>, Arc<AtomicBool>) {
        handle_from_http(MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: token_body.into(),
            },
        )]))
    }

    fn handle_from_http(
        http: Arc<MockHttp>,
    ) -> (OpenAiOAuthHandle, Arc<MemStorage>, Arc<AtomicBool>) {
        let clock = TestClock::new(1_000);
        let storage = MemStorage::new();
        let cm = mem_credential_manager(storage.clone(), clock.clone());
        let cfg = lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig::default();
        let handle = OpenAiOAuthHandle::new(cfg, http as Arc<dyn lingxi_llm_client::Transport>, cm)
            .with_clock(clock);

        let was_opened = Arc::new(AtomicBool::new(false));
        let browser_flag = was_opened.clone();

        // The test "browser opener" drives the loopback callback headlessly:
        // it parses the redirect_uri + state out of the authorize URL and
        // sends a synthetic GET /auth/callback?code=AUTHCODE&state=<state>
        // to the listener.
        let opener: BrowserOpener = Arc::new(move |url: &str| {
            browser_flag.store(true, Ordering::SeqCst);
            let url = url.to_string();
            tokio::spawn(async move {
                let (port, state) = parse_authorize_url(&url);
                for _ in 0..50 {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
                        let req = format!(
                            "GET /auth/callback?code=AUTHCODE&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
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

        (handle.with_browser_opener(opener), storage, was_opened)
    }

    /// Parse `redirect_uri` port and `state` out of an authorize URL.
    fn parse_authorize_url(url: &str) -> (u16, String) {
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
        // redirect_uri: "http://localhost:{port}/auth/callback"
        let port = redirect
            .rsplit_once(':')
            .and_then(|(_, rest)| rest.split('/').next())
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(0);
        (port, state)
    }

    // ── PKCE browser-flow tests ───────────────────────────────────────────

    /// A complete token-response body that includes an id_token with
    /// account_id, plus a second "call" for the api-key exchange.
    /// MockHttp matches by URL substring; "oauth/token" hits for both the
    /// code-exchange and the api-key RFC-8693 exchange, so we return a body
    /// that works for both (the api-key exchange ignores refresh_token/id_token
    /// and just reads `access_token`).
    fn full_token_body() -> String {
        // id_token payload: {"https://api.openai.com/auth":{"chatgpt_account_id":"acct_test","chatgpt_account_is_fedramp":false},"email":"u@example.com"}
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_test","chatgpt_account_is_fedramp":false},"email":"u@example.com"}"#,
        );
        let id_token = format!("{hdr}.{payload}.sig");
        format!(
            r#"{{"access_token":"bearer-tok","refresh_token":"ref-tok","expires_in":3600,"id_token":"{id_token}"}}"#
        )
    }

    /// Body for the api-key mint call (RFC-8693).
    /// We need the mock to return different bodies for code-exchange vs
    /// api-key-mint. Since MockHttp returns the SAME route for both (both hit
    /// "oauth/token"), we build a body that serves both:
    /// - `exchange_code_with_redirect` reads: access_token, refresh_token, id_token, expires_in
    /// - `obtain_api_key` reads: access_token (the minted key)
    ///
    /// We can't distinguish the two calls by URL alone, so we accept that
    /// `exchange_code_with_redirect` will parse "sk-test-key" as the access
    /// token (fine — handle only uses the result from the first call), and
    /// `obtain_api_key` will also get "sk-test-key" as access_token (the
    /// minted key). However, the id_token and refresh_token in the first call
    /// are what matter; the second call just needs `access_token`.
    ///
    /// SOLUTION: use two separate routes — the code exchange hits
    /// "oauth/token" first; the api-key mint hits "oauth/token" as well, so
    /// MockHttp returns the same body both times. We arrange the handle's
    /// flow so the first call (exchange) decodes the full body correctly, and
    /// the second call (obtain_api_key) also sees a valid `access_token` field.
    /// This works because `ExchangeResponse` uses `#[serde(default)]` for
    /// optional fields and `obtain_api_key` only reads `access_token`.
    ///
    /// So we only need ONE canned response that satisfies both parsers:
    fn unified_token_body() -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_test","chatgpt_account_is_fedramp":false}}"#,
        );
        let id_token = format!("{hdr}.{payload}.sig");
        // access_token serves as the "minted api key" for obtain_api_key,
        // and as the bearer token for exchange_code_with_redirect.
        format!(
            r#"{{"access_token":"sk-test-key","refresh_token":"ref-tok","expires_in":3600,"id_token":"{id_token}"}}"#
        )
    }

    fn handle_for_login() -> (OpenAiOAuthHandle, Arc<MemStorage>, Arc<AtomicBool>) {
        handle_with_token_body(&unified_token_body())
    }

    /// Same as [`handle_for_login`], but the RFC-8693 API-key mint 403s.
    ///
    /// Both exchanges POST to the same token URL, so the failure is keyed on
    /// the grant type in the request BODY.
    fn handle_with_failing_api_key_mint() -> (OpenAiOAuthHandle, Arc<MemStorage>, Arc<AtomicBool>) {
        handle_from_http(MockHttp::with_body_routes(
            vec![(
                "oauth/token",
                Canned {
                    status: 200,
                    body: unified_token_body(),
                },
            )],
            vec![(
                "grant-type%3Atoken-exchange",
                Canned {
                    status: 403,
                    body: r#"{"error":{"code":"api_key_not_available"}}"#.into(),
                },
            )],
        ))
    }

    #[tokio::test]
    async fn login_persists_tokens_and_api_key() {
        let _g = port_guard().await;
        let (handle, storage, opened) = handle_for_login();
        let info = handle.login().await.expect("login ok");
        assert!(opened.load(Ordering::SeqCst), "browser opener fired");
        assert_eq!(info.account_id.as_deref(), Some("acct_test"));
        assert!(!info.fedramp);
        assert_eq!(info.api_key, "sk-test-key");

        // Assert the exact slot NAMES, not just the count. A count is
        // slot-blind: writing the ChatGPT token into the Anthropic slots keeps
        // the total at 4, which is exactly how that defect shipped green out of
        // the refresh driver.
        assert_eq!(
            storage.accounts("lingxi"),
            vec![
                "openai-oauth-access".to_string(),
                "openai-oauth-meta".to_string(),
                "openai-oauth-refresh".to_string(),
                "provider-key-chatgpt".to_string(),
            ],
        );
    }

    /// A failed RFC-8693 API-key mint must not discard a completed sign-in.
    ///
    /// Accounts on a subscription-only ChatGPT plan cannot mint one at all;
    /// codex passes `.ok()` / `None` for exactly this reason. The OAuth tokens
    /// are what mobile inference actually uses.
    #[tokio::test]
    async fn login_survives_a_failed_api_key_mint() {
        let _g = port_guard().await;
        let (handle, storage, _) = handle_with_failing_api_key_mint();
        let info = handle.login().await.expect("login ok despite mint failure");
        assert_eq!(info.account_id.as_deref(), Some("acct_test"));
        assert_eq!(info.api_key, "", "no key minted");

        // The OAuth session is persisted; only the provider-key slot is absent.
        assert_eq!(
            storage.accounts("lingxi"),
            vec![
                "openai-oauth-access".to_string(),
                "openai-oauth-meta".to_string(),
                "openai-oauth-refresh".to_string(),
            ],
        );
        assert!(handle.current_user().await.is_some(), "session usable");
    }

    #[tokio::test]
    async fn current_user_reads_back_without_network() {
        let _g = port_guard().await;
        let (handle, _storage, _) = handle_for_login();
        handle.login().await.expect("login ok");

        let cu = handle.current_user().await.expect("current_user present");
        assert_eq!(cu.account_id.as_deref(), Some("acct_test"));
        assert_eq!(cu.api_key, "sk-test-key");
    }

    #[tokio::test]
    async fn logout_clears_and_current_user_is_none() {
        let _g = port_guard().await;
        let (handle, storage, _) = handle_for_login();
        handle.login().await.expect("login ok");
        assert_eq!(storage.count("lingxi"), 4);
        assert!(
            storage
                .accounts("lingxi")
                .iter()
                .all(|a| !a.starts_with("anthropic-")),
            "login wrote an Anthropic slot: {:?}",
            storage.accounts("lingxi"),
        );

        handle.logout().await.expect("logout ok");
        // OpenAI OAuth slots cleared; provider-key-chatgpt overwritten with "".
        // Storage still has the overwritten provider-key entry, but it's empty.
        assert!(handle.current_user().await.is_none());

        // Idempotent second logout.
        handle.logout().await.expect("second logout ok");
    }

    #[tokio::test]
    async fn current_user_none_when_not_logged_in() {
        let (handle, _storage, _) = handle_for_login();
        assert!(handle.current_user().await.is_none());
    }
}
