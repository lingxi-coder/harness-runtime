//! OAuth token refresh: reactive (401-driven) + proactive (timer-driven).
//!
//! Ported from `anthropic-oauth/src/refresh.rs` and adapted for `OpenAI`:
//! - No `scope` param in the refresh POST (`OpenAI` doesn't accept it).
//! - Response carries `id_token?` — when present we update `account_id`/`fedramp`.
//! - Proactive refresh: refresh when `expires_at - now <= 5 minutes` OR
//!   `last_refresh` older than 8 days.
//! - `TokenInfo` additionally holds `account_id: Option<String>` and `fedramp: bool`.

#![allow(dead_code)]

use crate::auth::lifecycle::{self, OAuthHookError, RefreshableToken, TokenHash};
use crate::auth::openai::login::OAuthError;
use crate::auth::token_manager::{
    self, OAuthSession, OAuthTokenManager, RefreshEvents, SharedRefreshLock, SESSION_EXPIRED,
};
use async_trait::async_trait;
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use lingxi_llm_client::auth::oauth::openai::{
    refresh_token, ExchangedTokens as TokenEndpointResponse, OAuthProtocolError,
};
use lingxi_llm_client::{
    transport::{HttpRequest as SdkHttpRequest, StreamResponse},
    Transport,
};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, RwLock};

/// Threshold: refresh proactively when expiry is within 5 minutes.
pub const PROACTIVE_LEAD_CAP: Duration = Duration::from_secs(5 * 60);

/// Proactive refresh also triggers when the last successful refresh was more
/// than 8 days ago (even if the access token is still technically valid, the
/// `refresh_token` may have rotated).
const LAST_REFRESH_MAX_AGE: Duration = Duration::from_secs(8 * 24 * 60 * 60);

/// In-memory token state.
pub struct TokenInfo {
    /// Bearer access token.
    pub access_token: Secret<String>,
    /// Refresh token.
    pub refresh_token: Option<Secret<String>>,
    /// Expiry instant.
    pub expires_at: SystemTime,
    /// `ChatGPT` workspace/account id (from `id_token` claims).
    pub account_id: Option<String>,
    /// `FedRAMP` account flag (from `id_token` claims).
    pub fedramp: bool,
    /// Signed-in email (from `id_token` claims), for display only.
    pub email: Option<String>,
    /// Wall-clock time of the last successful refresh (for 8-day check).
    pub last_refresh: Option<SystemTime>,
}

impl TokenInfo {
    /// SHA-256 of the `access_token` bytes.
    #[must_use]
    pub fn token_hash(&self) -> TokenHash {
        lifecycle::token_hash(&self.access_token)
    }
}

impl RefreshableToken for TokenInfo {
    fn access_token(&self) -> &Secret<String> {
        &self.access_token
    }
    fn refresh_token(&self) -> Option<&Secret<String>> {
        self.refresh_token.as_ref()
    }
}

/// Shared OAuth state.
pub struct AuthState {
    pub(crate) config: OpenAiOAuthConfig,
    /// Current token under `RwLock` so reactive readers don't serialize.
    pub token: RwLock<TokenInfo>,
    /// Single-flight refresh lock.
    pub(crate) refresh_lock: Arc<Mutex<()>>,
    /// Proactive task handle.
    pub(crate) proactive_handle: RwLock<Option<lingxi_core::host::BackgroundTaskHandle>>,
    pub(crate) http: Arc<dyn Transport>,
    pub(crate) clock: Arc<dyn lingxi_core::host::Clock>,
    pub(crate) bus: Option<Arc<telemetry::AnalyticsBus>>,
    pub(crate) credentials: Option<Arc<secret::CredentialManager>>,
    /// Shared with other processes that store this session.
    pub(crate) shared_refresh: OnceLock<Arc<dyn SharedRefreshLock>>,
}

impl AuthState {
    /// Production constructor.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        config: OpenAiOAuthConfig,
        access_token: Secret<String>,
        refresh_token: Option<Secret<String>>,
        expires_at: SystemTime,
        account_id: Option<String>,
        fedramp: bool,
        email: Option<String>,
        http: Arc<dyn Transport>,
        clock: Arc<dyn lingxi_core::host::Clock>,
        bus: Option<Arc<telemetry::AnalyticsBus>>,
        credentials: Option<Arc<secret::CredentialManager>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            token: RwLock::new(TokenInfo {
                access_token,
                refresh_token,
                expires_at,
                account_id,
                fedramp,
                email,
                last_refresh: None,
            }),
            refresh_lock: Arc::new(Mutex::new(())),
            proactive_handle: RwLock::new(None),
            http,
            clock,
            bus,
            credentials,
            shared_refresh: OnceLock::new(),
        })
    }

    /// Test-only constructor.
    #[must_use]
    pub fn new_for_test(
        config: OpenAiOAuthConfig,
        access_token: Secret<String>,
        refresh_token: Option<Secret<String>>,
        expires_at: SystemTime,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            token: RwLock::new(TokenInfo {
                access_token,
                refresh_token,
                expires_at,
                account_id: None,
                fedramp: false,
                email: None,
                last_refresh: None,
            }),
            refresh_lock: Arc::new(Mutex::new(())),
            proactive_handle: RwLock::new(None),
            http: Arc::new(NullTransport),
            clock: Arc::new(NullClock),
            bus: None,
            credentials: None,
            shared_refresh: OnceLock::new(),
        })
    }

    /// Borrow the proactive task handle if one has been spawned.
    pub async fn proactive_handle(&self) -> Option<lingxi_core::host::BackgroundTaskHandle> {
        self.proactive_handle.read().await.clone()
    }

    /// Cancel the proactive refresh task.
    pub async fn shutdown(&self, spawner: &dyn lingxi_core::host::RuntimeSpawner) {
        token_manager::shutdown(self, spawner).await;
    }

    /// Invalidate the in-memory token after persisted credentials are removed.
    /// This prevents an already-built engine from continuing to authenticate
    /// until its source is rebuilt.
    pub async fn invalidate(&self) {
        token_manager::invalidate(self).await;
    }

    /// Refresh only under `lock`, and adopt a session another process sharing
    /// the store already rotated. The first lock wins.
    pub fn share_refresh_across_processes(&self, lock: Arc<dyn SharedRefreshLock>) {
        let _ = self.shared_refresh.set(lock);
    }

    async fn do_refresh_http(
        &self,
        refresh: &Secret<String>,
    ) -> Result<TokenEndpointResponse, OAuthError> {
        match refresh_token(self.http.as_ref(), &self.config, refresh.expose_secret()).await {
            Ok(tokens) => Ok(tokens),
            Err(OAuthProtocolError::InvalidRefreshCredential) => Err(OAuthError::RefreshExpired),
            Err(e) => Err(OAuthError::TokenExchange(e.to_string())),
        }
    }

    /// Persist the rotated token to the keychain via `CredentialManager`.
    /// No-op when no manager is configured (test path).
    ///
    /// Writes the `openai-oauth-*` slots. It must NOT touch `store_oauth_tokens`
    /// / `get_oauth_tokens`: those are the ANTHROPIC slots
    /// (`anthropic-oauth-access|-refresh|-meta`, `secret::credential`), and this
    /// driver fires after every ChatGPT rotation — writing there overwrote the
    /// user's Claude session roughly an hour after any ChatGPT login while
    /// leaving the stale ChatGPT token behind in `openai-oauth-access`.
    ///
    /// The identity is taken from the in-memory `TokenInfo`, which the caller
    /// has already refreshed from the new `id_token` claims (or carried
    /// forward when the response had none), so no read-back is needed. `scopes`
    /// is `vec![]` to match the login path (`OpenAiOAuthHandle`'s three persist
    /// sites); nothing reads OpenAI scopes back.
    async fn persist_to_keychain(&self, info: &TokenInfo) -> Result<(), OAuthError> {
        let Some(cm) = &self.credentials else {
            return Ok(());
        };
        let refresh = info
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        cm.store_openai_oauth_tokens(
            info.access_token.expose_secret(),
            refresh.as_deref(),
            info.expires_at,
            vec![],
            info.account_id.as_deref(),
            info.fedramp,
            info.email.as_deref(),
        )
        .await
        .map_err(|e| OAuthError::TokenExchange(format!("keychain store: {e}")))?;
        Ok(())
    }
}

/// Null transport used by test-only state.
struct NullTransport;
#[async_trait]
impl Transport for NullTransport {
    async fn send(
        &self,
        _req: SdkHttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        panic!("NullTransport: test forgot to inject a real transport");
    }
}

/// Null clock — always returns [`SystemTime::UNIX_EPOCH`].
struct NullClock;
impl lingxi_core::host::Clock for NullClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
}

/// The OpenAI half of a ChatGPT session; [`OAuthTokenManager`] does the rest.
#[async_trait]
impl OAuthSession for AuthState {
    type Token = TokenInfo;

    fn events(&self) -> &'static RefreshEvents {
        &EVENTS
    }

    fn token(&self) -> &RwLock<TokenInfo> {
        &self.token
    }

    fn refresh_lock(&self) -> &Arc<Mutex<()>> {
        &self.refresh_lock
    }

    fn proactive_task(&self) -> &RwLock<Option<lingxi_core::host::BackgroundTaskHandle>> {
        &self.proactive_handle
    }

    fn now(&self) -> SystemTime {
        self.clock.now()
    }

    fn bus(&self) -> &Option<Arc<telemetry::AnalyticsBus>> {
        &self.bus
    }

    fn shared_lock(&self) -> Option<&Arc<dyn SharedRefreshLock>> {
        self.shared_refresh.get()
    }

    fn expires_at(token: &TokenInfo) -> SystemTime {
        token.expires_at
    }

    /// Before expiry by `min(remaining/2, 5min)`, or once the last refresh is
    /// 8 days old, whichever comes first.
    fn refresh_due_in(token: &TokenInfo, now: SystemTime) -> Duration {
        let remaining = token
            .expires_at
            .duration_since(now)
            .unwrap_or(Duration::ZERO);
        let until_expiry_lead = remaining
            .checked_sub(proactive_lead(remaining))
            .unwrap_or(Duration::ZERO);
        match token.last_refresh {
            Some(last) => {
                let age = now.duration_since(last).unwrap_or(Duration::ZERO);
                LAST_REFRESH_MAX_AGE
                    .checked_sub(age)
                    .unwrap_or(Duration::ZERO)
                    .min(until_expiry_lead)
            }
            None => until_expiry_lead,
        }
    }

    async fn exchange(&self, refresh_token: Secret<String>) -> Result<TokenInfo, OAuthHookError> {
        let body = match self.do_refresh_http(&refresh_token).await {
            Ok(body) => body,
            Err(OAuthError::RefreshExpired) => {
                return Err(OAuthHookError::RefreshFailed(SESSION_EXPIRED.into()));
            }
            Err(e) => return Err(OAuthHookError::ProviderUnreachable(format!("{e}"))),
        };

        let now = self.clock.now();
        // Update account_id/fedramp/email from id_token if present.
        let (account_id, fedramp, email) = {
            let current = self.token.read().await;
            match body
                .id_token
                .as_deref()
                .map(lingxi_llm_client::auth::oauth::openai::parse_id_token)
            {
                // An id_token that omits `email` must not erase the identity
                // captured at login: the claim is stable for a given account,
                // and the refresh response sometimes drops it.
                Some(Some(claims)) => (
                    claims.account_id,
                    claims.fedramp,
                    claims.email.or_else(|| current.email.clone()),
                ),
                // No id_token, or one that does not parse: keep what we have.
                _ => (
                    current.account_id.clone(),
                    current.fedramp,
                    current.email.clone(),
                ),
            }
        };

        Ok(TokenInfo {
            access_token: Secret::new(body.access_token.clone()),
            refresh_token: body
                .refresh_token
                .clone()
                .map(Secret::new)
                .or(Some(refresh_token)),
            expires_at: now + body.effective_lifetime(),
            account_id,
            fedramp,
            email,
            last_refresh: Some(now),
        })
    }

    async fn persist(&self, token: &TokenInfo) -> Result<(), String> {
        self.persist_to_keychain(token)
            .await
            .map_err(|e| e.to_string())
    }

    async fn stored(&self) -> Option<TokenInfo> {
        let stored = self
            .credentials
            .as_ref()?
            .get_openai_oauth_tokens()
            .await
            .ok()??;
        Some(TokenInfo {
            access_token: stored.access_token,
            refresh_token: stored.refresh_token,
            expires_at: stored.expires_at,
            account_id: stored.account_id,
            fedramp: stored.fedramp,
            email: stored.email,
            last_refresh: None,
        })
    }

    fn clear(token: &mut TokenInfo) {
        token.access_token = Secret::new(String::new());
        token.refresh_token = None;
        token.expires_at = SystemTime::UNIX_EPOCH;
        token.account_id = None;
        token.fedramp = false;
        token.last_refresh = None;
    }
}

static EVENTS: RefreshEvents = RefreshEvents {
    started: "tengu_openai_oauth_refresh_started",
    succeeded: "tengu_openai_oauth_refresh_succeeded",
    failed: "tengu_openai_oauth_refresh_failed",
    proactive_canceled: "tengu_openai_oauth_proactive_canceled",
    proactive_task: "lingxi-openai-oauth-proactive-refresh",
    provider: "openai",
};

/// Drives reactive + proactive refresh of a ChatGPT session.
pub type RefreshDriver = OAuthTokenManager<AuthState>;

/// Compute the proactive refresh lead for a token with `remaining` lifetime.
///
/// Returns `min(remaining / 2, PROACTIVE_LEAD_CAP)` in whole seconds.
#[must_use]
pub fn proactive_lead(remaining: Duration) -> Duration {
    lifecycle::proactive_lead(remaining, PROACTIVE_LEAD_CAP)
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use crate::auth::openai::testsupport::{
        mem_credential_manager, Canned, MemStorage, MockHttp, TestClock,
    };

    /// A rotated ChatGPT token must land in the `openai-oauth-*` slots and
    /// must not touch `anthropic-oauth-*`.
    ///
    /// `persist_to_keychain` used to call `store_oauth_tokens`, which is the
    /// ANTHROPIC writer — so roughly an hour after any ChatGPT login the
    /// proactive refresh clobbered the user's Claude session while leaving the
    /// stale ChatGPT token in place. Asserting the slot NAMES (not the entry
    /// count) is what makes this detectable: the buggy write kept the total
    /// unchanged.
    #[tokio::test]
    async fn rotated_chatgpt_token_lands_in_the_openai_slot_only() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: r#"{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600}"#
                    .into(),
            },
        )]);
        let clock = TestClock::new(2_000);
        let storage = MemStorage::new();
        let credentials = mem_credential_manager(
            storage.clone(),
            clock.clone() as Arc<dyn lingxi_core::host::Clock>,
        );

        let state = AuthState::new(
            OpenAiOAuthConfig::default(),
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_010),
            Some("acc_XYZ".into()),
            false,
            Some("acc_xyz@example.com".into()),
            http.clone() as Arc<dyn Transport>,
            clock.clone() as Arc<dyn lingxi_core::host::Clock>,
            None,
            Some(credentials.clone()),
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let accounts = storage.accounts("lingxi");
        assert!(
            accounts.iter().all(|a| !a.starts_with("anthropic-")),
            "ChatGPT rotation wrote an Anthropic slot: {accounts:?}"
        );
        assert!(
            accounts.iter().any(|a| a == "openai-oauth-access"),
            "rotated token never reached the OpenAI slot: {accounts:?}"
        );

        // And the value actually rotated in the OpenAI slot.
        let stored = credentials
            .get_openai_oauth_tokens()
            .await
            .expect("read openai slot")
            .expect("session present");
        assert_eq!(stored.access_token.expose_secret(), "NEW_ACCESS");
        assert_eq!(stored.account_id.as_deref(), Some("acc_XYZ"));
    }

    #[tokio::test]
    async fn reactive_refresh_sends_json_body_and_rotates_token() {
        let resp =
            r#"{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600}"#;
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp.into(),
            },
        )]);
        let clock = TestClock::new(2_000);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_010),
            None,
            false,
            None,
            http.clone() as Arc<dyn Transport>,
            clock.clone() as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();

        let token = driver.refresh(prev).await.expect("refresh ok");
        assert_eq!(token.0.expose_secret(), "NEW_ACCESS");
        assert_eq!(http.call_count(), 1);

        // Wire shape: POST JSON with grant_type=refresh_token.
        let req = http.last_request().expect("request made");
        assert_eq!(req.method, "POST");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json"));
        let sent: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&req.body).unwrap()).expect("json body");
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "OLD_REFRESH");
        assert_eq!(sent["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        // OpenAI does NOT send scope param.
        assert!(sent.get("scope").is_none());

        // Token in-memory rotated.
        let t = state.token.read().await;
        assert_eq!(t.access_token.expose_secret(), "NEW_ACCESS");
        assert_eq!(
            t.expires_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_000 + 3_600)
        );
    }

    #[tokio::test]
    async fn reactive_refresh_updates_account_id_from_id_token() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        // Build a minimal id_token with account_id and fedramp claims.
        let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_XYZ","chatgpt_account_is_fedramp":true}}"#,
        );
        let id_token = format!("{hdr}.{payload}.sig");
        let resp = format!(
            r#"{{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600,"id_token":"{id_token}"}}"#
        );
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp,
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            None,
            false,
            None,
            http as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let t = state.token.read().await;
        assert_eq!(t.account_id.as_deref(), Some("acc_XYZ"));
        assert!(t.fedramp);
    }

    /// A rotation response whose `id_token` omits `email` must not erase the
    /// address captured at login — the refresh endpoint routinely returns a
    /// narrower claim set than the original authorization-code exchange, and
    /// the settings page would otherwise lose the account name an hour in.
    #[tokio::test]
    async fn reactive_refresh_keeps_email_when_id_token_omits_it() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_XYZ"}}"#);
        let id_token = format!("{hdr}.{payload}.sig");
        let resp = format!(
            r#"{{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600,"id_token":"{id_token}"}}"#
        );
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp,
            },
        )]);
        let clock = TestClock::new(0);
        let state = AuthState::new(
            OpenAiOAuthConfig::default(),
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            Some("acc_XYZ".into()),
            false,
            Some("user@example.com".into()),
            http as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let t = state.token.read().await;
        assert_eq!(t.account_id.as_deref(), Some("acc_XYZ"));
        assert_eq!(t.email.as_deref(), Some("user@example.com"));
    }

    /// When the refresh `id_token` does carry `email`, it wins over the
    /// carried-forward value (a rare but real case: the account switched
    /// addresses between rotations).
    #[tokio::test]
    async fn reactive_refresh_takes_email_from_id_token_when_present() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_XYZ"},"email":"new@example.com"}"#,
        );
        let id_token = format!("{hdr}.{payload}.sig");
        let resp = format!(
            r#"{{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600,"id_token":"{id_token}"}}"#
        );
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp,
            },
        )]);
        let clock = TestClock::new(0);
        let state = AuthState::new(
            OpenAiOAuthConfig::default(),
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            Some("acc_XYZ".into()),
            false,
            Some("old@example.com".into()),
            http as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let t = state.token.read().await;
        assert_eq!(t.email.as_deref(), Some("new@example.com"));
    }

    #[tokio::test]
    async fn reactive_refresh_invalid_grant_maps_to_session_expired() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            None,
            false,
            None,
            http as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        let err = driver.refresh(prev).await.expect_err("401 must fail");
        match err {
            OAuthHookError::RefreshFailed(msg) => {
                assert_eq!(msg, "Session expired. Re-authenticate?");
            }
            other => panic!("expected RefreshFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_request_uses_15s_timeout() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: r#"{"access_token":"NEW_ACCESS","expires_in":3600}"#.into(),
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            None,
            false,
            None,
            http.clone() as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let req = http.last_request().expect("request made");
        assert!(req
            .timeout
            .is_some_and(|t| t > Duration::ZERO && t <= Duration::from_secs(15)));
    }

    #[tokio::test]
    async fn proactive_loop_fires_then_exits_on_401() {
        use crate::auth::openai::testsupport::InstantSpawner;

        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            None,
            false,
            None,
            http.clone() as Arc<dyn Transport>,
            clock.clone() as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let spawner = InstantSpawner::new();
        RefreshDriver::spawn_proactive(
            state.clone(),
            spawner.clone() as Arc<dyn lingxi_core::host::RuntimeSpawner>,
        )
        .await
        .expect("spawn ok");

        clock.set(2);

        for _ in 0..100 {
            if http.call_count() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            http.call_count(),
            1,
            "proactive fired once and exited on 401 (no spin)"
        );
    }
}
