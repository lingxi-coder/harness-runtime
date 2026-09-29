//! OAuth token refresh: reactive (401-driven) + proactive (timer-driven).
//!
//! Ported from `anthropic-oauth/src/refresh.rs` and adapted for `OpenAI`:
//! - No `scope` param in the refresh POST (`OpenAI` doesn't accept it).
//! - Response carries `id_token?` — when present we update `account_id`/`fedramp`.
//! - Proactive refresh: refresh when `expires_at - now <= 5 minutes` OR
//!   `last_refresh` older than 8 days.
//! - `TokenInfo` additionally holds `account_id: Option<String>` and `fedramp: bool`.

#![allow(dead_code)]

use crate::auth::lifecycle::{
    self, BearerToken, OAuthHookError, Preflight, RefreshableToken, TokenHash,
};
use crate::auth::openai::login::OAuthError;
use async_trait::async_trait;
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use lingxi_llm_client::auth::oauth::openai::{
    refresh_token, ExchangedTokens as TokenEndpointResponse, OAuthProtocolError,
};
use lingxi_llm_client::{
    transport::{HttpRequest as SdkHttpRequest, StreamResponse},
    Transport,
};
use protocol::Secret;
use std::sync::Arc;
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
    pub(crate) proactive_handle: RwLock<Option<platform_api::BackgroundTaskHandle>>,
    pub(crate) http: Arc<dyn Transport>,
    pub(crate) clock: Arc<dyn platform_api::Clock>,
    pub(crate) bus: Option<Arc<telemetry::AnalyticsBus>>,
    pub(crate) credentials: Option<Arc<secret::CredentialManager>>,
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
        clock: Arc<dyn platform_api::Clock>,
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
        })
    }

    /// Borrow the proactive task handle if one has been spawned.
    pub async fn proactive_handle(&self) -> Option<platform_api::BackgroundTaskHandle> {
        self.proactive_handle.read().await.clone()
    }

    /// Cancel the proactive refresh task.
    pub async fn shutdown(&self, spawner: &dyn platform_api::RuntimeSpawner) {
        let handle = self.proactive_handle.write().await.take();
        let Some(handle) = handle else {
            return;
        };
        if let Err(e) = spawner.cancel(&handle).await {
            tracing::warn!(
                target: "lingxi::openai_oauth::shutdown",
                error = ?e,
                task_name = %handle.task_name,
                "proactive task cancel returned error; treating as no-op",
            );
        }
        emit_proactive_canceled(&self.bus, "engine_shutdown").await;
    }

    /// Invalidate the in-memory token after persisted credentials are removed.
    /// This prevents an already-built engine from continuing to authenticate
    /// until its source is rebuilt.
    pub async fn invalidate(&self) {
        let mut token = self.token.write().await;
        token.access_token = Secret::new(String::new());
        token.refresh_token = None;
        token.expires_at = SystemTime::UNIX_EPOCH;
        token.account_id = None;
        token.fedramp = false;
        token.last_refresh = None;
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
impl platform_api::Clock for NullClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
}

/// Drives reactive + proactive refresh.
pub struct RefreshDriver {
    pub(crate) state: Arc<AuthState>,
}

impl RefreshDriver {
    /// Construct a driver over a shared `AuthState`.
    #[must_use]
    pub fn new(state: Arc<AuthState>) -> Self {
        Self { state }
    }

    /// Stop proactive refresh and invalidate this driver's in-memory token.
    pub async fn invalidate(&self, spawner: &dyn platform_api::RuntimeSpawner) {
        self.state.shutdown(spawner).await;
        self.state.invalidate().await;
    }
}

impl RefreshDriver {
    /// Perform a single-flight OAuth token refresh.
    ///
    /// **Single-flight contract:** acquires `refresh_lock`, then double-checks
    /// `prev_token_hash`. If another task already rotated the token under the
    /// lock, returns the current token without making an HTTP call.
    pub async fn refresh(&self, prev_token_hash: TokenHash) -> Result<BearerToken, OAuthHookError> {
        // Keep this guard through the network request and state rotation.
        let _guard = self.state.refresh_lock.lock().await;
        let refresh_token = match lifecycle::preflight(&self.state.token, prev_token_hash).await? {
            Preflight::AlreadyRotated(bearer) => return Ok(bearer),
            Preflight::RefreshWith(token) => token,
        };

        let started = std::time::Instant::now();
        emit_refresh_started(&self.state.bus, "reactive_401").await;

        // 4. Perform the HTTP refresh.
        let body = match self.state.do_refresh_http(&refresh_token).await {
            Ok(b) => b,
            Err(OAuthError::RefreshExpired) => {
                emit_refresh_failed(&self.state.bus, "reactive_401", "refresh_expired").await;
                return Err(OAuthHookError::RefreshFailed(
                    "Session expired. Re-authenticate?".into(),
                ));
            }
            Err(e) => {
                emit_refresh_failed(&self.state.bus, "reactive_401", "provider_unreachable").await;
                return Err(OAuthHookError::ProviderUnreachable(format!("{e}")));
            }
        };

        // 5. Build the new TokenInfo.
        let now = self.state.clock.now();
        let new_expiry = now + body.effective_lifetime();

        // Update account_id/fedramp/email from id_token if present.
        let (new_account_id, new_fedramp, new_email) = if let Some(ref id_token) = body.id_token {
            if let Some(claims) = lingxi_llm_client::auth::oauth::openai::parse_id_token(id_token) {
                // An id_token that omits `email` must not erase the identity
                // captured at login: the claim is stable for a given account,
                // and the refresh response sometimes drops it.
                let previous = self.state.token.read().await.email.clone();
                (claims.account_id, claims.fedramp, claims.email.or(previous))
            } else {
                // id_token present but unparseable — preserve existing values.
                let t = self.state.token.read().await;
                (t.account_id.clone(), t.fedramp, t.email.clone())
            }
        } else {
            // No id_token in response — preserve existing values.
            let t = self.state.token.read().await;
            (t.account_id.clone(), t.fedramp, t.email.clone())
        };

        let new_access_token_str = body.access_token.clone();
        let new_info = TokenInfo {
            access_token: Secret::new(body.access_token),
            refresh_token: body.refresh_token.map(Secret::new).or(Some(refresh_token)),
            expires_at: new_expiry,
            account_id: new_account_id,
            fedramp: new_fedramp,
            email: new_email,
            last_refresh: Some(now),
        };

        // 6. Atomic swap.
        {
            let mut guard = self.state.token.write().await;
            *guard = new_info;
        }

        // 7. Persist to keychain (best-effort).
        if let Err(e) = self
            .state
            .persist_to_keychain(&*self.state.token.read().await)
            .await
        {
            tracing::warn!(
                target: "lingxi::openai_oauth::refresh",
                error = %e,
                "keychain persistence failed; in-memory token is still rotated",
            );
        }

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let new_expiry_unix = i64::try_from(
            new_expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .unwrap_or(i64::MAX);
        emit_refresh_succeeded(
            &self.state.bus,
            "reactive_401",
            new_expiry_unix,
            duration_ms,
        )
        .await;

        Ok(BearerToken(Secret::new(new_access_token_str)))
    }
}

// Provider event names stay distinct; metadata construction is shared.
async fn emit_refresh_started(bus: &Option<Arc<telemetry::AnalyticsBus>>, trigger: &str) {
    lifecycle::emit_refresh_started(bus, "tengu_openai_oauth_refresh_started", trigger).await;
}

async fn emit_refresh_succeeded(
    bus: &Option<Arc<telemetry::AnalyticsBus>>,
    trigger: &str,
    new_expiry_unix: i64,
    duration_ms: u64,
) {
    lifecycle::emit_refresh_succeeded(
        bus,
        "tengu_openai_oauth_refresh_succeeded",
        trigger,
        new_expiry_unix,
        duration_ms,
    )
    .await;
}

async fn emit_refresh_failed(
    bus: &Option<Arc<telemetry::AnalyticsBus>>,
    trigger: &str,
    error_kind: &str,
) {
    lifecycle::emit_refresh_failed(
        bus,
        "tengu_openai_oauth_refresh_failed",
        trigger,
        error_kind,
    )
    .await;
}

async fn emit_proactive_canceled(bus: &Option<Arc<telemetry::AnalyticsBus>>, reason: &str) {
    lifecycle::emit_proactive_canceled(bus, "tengu_openai_oauth_proactive_canceled", reason).await;
}

/// Compute the proactive refresh lead for a token with `remaining` lifetime.
///
/// Returns `min(remaining / 2, PROACTIVE_LEAD_CAP)` in whole seconds.
#[must_use]
pub fn proactive_lead(remaining: Duration) -> Duration {
    lifecycle::proactive_lead(remaining, PROACTIVE_LEAD_CAP)
}

impl RefreshDriver {
    /// Spawn the proactive refresh task.
    pub async fn spawn_proactive(
        state: Arc<AuthState>,
        spawner: Arc<dyn platform_api::RuntimeSpawner>,
    ) -> Result<(), OAuthError> {
        let task_state = state.clone();
        let task_spawner = spawner.clone();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> =
            Box::pin(async move {
                proactive_loop(task_state, task_spawner).await;
            });
        let handle = spawner
            .spawn("lingxi-openai-oauth-proactive-refresh", fut)
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("spawn failed: {e}")))?;
        *state.proactive_handle.write().await = Some(handle);
        Ok(())
    }
}

/// The proactive task loop. Wakes at `min(remaining/2, 5min)` before expiry.
/// Also fires early if `last_refresh` is older than 8 days.
async fn proactive_loop(state: Arc<AuthState>, spawner: Arc<dyn platform_api::RuntimeSpawner>) {
    let driver = RefreshDriver::new(state.clone());
    loop {
        // Read current expiry + token_hash + last_refresh.
        let (expires_at, prev_hash, last_refresh) = {
            let t = state.token.read().await;
            (t.expires_at, t.token_hash(), t.last_refresh)
        };
        let now = state.clock.now();
        let remaining = expires_at.duration_since(now).unwrap_or(Duration::ZERO);
        let lead = proactive_lead(remaining);
        let time_until_expiry_wake = remaining.checked_sub(lead).unwrap_or(Duration::ZERO);

        // 8-day proactive refresh: if last_refresh is old, wake sooner.
        let sleep_for = if let Some(lr) = last_refresh {
            let age = now.duration_since(lr).unwrap_or(Duration::ZERO);
            if age >= LAST_REFRESH_MAX_AGE {
                // Refresh immediately.
                Duration::ZERO
            } else {
                // Wake at the earlier of expiry-lead or 8-day-age trigger.
                let time_until_age_trigger = LAST_REFRESH_MAX_AGE
                    .checked_sub(age)
                    .unwrap_or(Duration::ZERO);
                time_until_expiry_wake.min(time_until_age_trigger)
            }
        } else {
            time_until_expiry_wake
        };

        emit_refresh_started(&state.bus, "proactive_timer").await;

        spawner.sleep(sleep_for).await;

        match driver.refresh(prev_hash).await {
            Ok(_) => {
                continue;
            }
            Err(OAuthHookError::RefreshFailed(msg))
                if msg == "Session expired. Re-authenticate?" =>
            {
                tracing::error!(
                    target: "lingxi::openai_oauth::proactive",
                    "refresh_token expired; exiting proactive loop"
                );
                emit_refresh_failed(&state.bus, "proactive_timer", "refresh_expired").await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "lingxi::openai_oauth::proactive",
                    error = ?e,
                    "transient refresh failure; backing off 30s",
                );
                emit_refresh_failed(&state.bus, "proactive_timer", "provider_unreachable").await;
                spawner.sleep(Duration::from_secs(30)).await;
                continue;
            }
        }
    }
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
            clock.clone() as Arc<dyn platform_api::Clock>,
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
            clock.clone() as Arc<dyn platform_api::Clock>,
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
            clock.clone() as Arc<dyn platform_api::Clock>,
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
            clock as Arc<dyn platform_api::Clock>,
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
            clock as Arc<dyn platform_api::Clock>,
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
            clock as Arc<dyn platform_api::Clock>,
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
            clock as Arc<dyn platform_api::Clock>,
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
            clock as Arc<dyn platform_api::Clock>,
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
            clock.clone() as Arc<dyn platform_api::Clock>,
            None,
            None,
        );
        let spawner = InstantSpawner::new();
        RefreshDriver::spawn_proactive(
            state.clone(),
            spawner.clone() as Arc<dyn platform_api::RuntimeSpawner>,
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
