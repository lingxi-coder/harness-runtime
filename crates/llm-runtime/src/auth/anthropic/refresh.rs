//! OAuth token refresh: reactive (401-driven) + proactive (timer-driven).
//!
//! See spec §3 (cross-plan trait), §4 Flow A (lifecycle), §7 (wire identifiers),
//! §8 M3-04 phase list.
//!
//! The single-flight invariant is enforced via [`AuthState::refresh_lock`]
//! with double-check-after-acquire (v3 §16.3).
//!
//! Host refresh errors and token coordination are shared by both OAuth providers.
//! The credential seam handles token refresh via `OAuthCredentialProvider`.

// Task 2 lands the data model; Task 4+ consumes these fields via the
// inherent `refresh` + `spawn_proactive`. Allow until then.
#![allow(dead_code)]

use crate::auth::anthropic::login::OAuthError;
use crate::auth::lifecycle::{
    self, BearerToken, OAuthHookError, Preflight, RefreshableToken, TokenHash,
};
use async_trait::async_trait;
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::auth::oauth::anthropic::{self as sdk, RefreshResponse, TokenError};
use lingxi_llm_client::transport::Transport;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, RwLock};

/// Timeout for the refresh-token POST. Matches claude-code's
/// `refreshOAuthToken` deadline of `timeout: 15000` (15s)
/// (services/oauth/client.ts:168), the same 15s the exchange path uses.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// In-memory token state. Kept under `AuthState::token` (`RwLock`) and atomically
/// swapped on a successful refresh.
pub struct TokenInfo {
    /// Bearer access token. Wrapped in `Secret` so `Debug`/`Display` redact.
    pub access_token: Secret<String>,
    /// Refresh token (optional — some upstream flows return only an access token).
    pub refresh_token: Option<Secret<String>>,
    /// Expiry instant (wall clock); the proactive task wakes at
    /// `min((expires_at - now)/2, 5*60)` seconds before this.
    pub expires_at: SystemTime,
    /// Scopes currently granted. Used to detect scope-upgrade transitions.
    pub scopes: Vec<String>,
}

impl TokenInfo {
    /// SHA-256 of the `access_token` bytes. Returned as the `TokenHash` newtype
    /// M3-03 declared (`pub [u8; 32]`).
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

/// Shared OAuth state. Holds the current token, the single-flight refresh lock,
/// the OAuth config, the I/O collaborators (HTTP transport, clock, optional
/// telemetry + keychain), and the proactive task handle (when spawned).
pub struct AuthState {
    /// OAuth config (endpoints + `client_id` + redirect + scopes).
    pub(crate) config: ClaudeAiOAuthConfig,
    /// Current token under `RwLock` so reactive readers don't serialize.
    pub token: RwLock<TokenInfo>,
    /// Single-flight refresh lock (v3 §16.3). Lock value is `()`; semantic is
    /// critical-section serialization across reactive + proactive paths.
    pub(crate) refresh_lock: Arc<Mutex<()>>,
    /// Proactive task handle. Populated by `RefreshDriver::spawn_proactive`;
    /// cleared by `AuthState::shutdown`.
    pub(crate) proactive_handle: RwLock<Option<lingxi_core::host::BackgroundTaskHandle>>,
    /// HTTP transport for token-endpoint POSTs. Engine code never imports a
    /// concrete HTTP client; we go through the trait per D17.
    pub(crate) http: Arc<dyn Transport>,
    /// Wall-clock source. Tests inject a virtual clock.
    pub(crate) clock: Arc<dyn lingxi_core::host::Clock>,
    /// Optional analytics bus for `tengu_oauth_*` events. `None` in tests that
    /// don't care about telemetry.
    pub(crate) bus: Option<Arc<telemetry::AnalyticsBus>>,
    /// Optional credential manager for keychain persistence. `None` in tests
    /// that only exercise the in-memory token rotation.
    pub(crate) credentials: Option<Arc<secret::CredentialManager>>,
}

impl AuthState {
    /// Production constructor. Wires HTTP + clock + telemetry + keychain.
    ///
    /// The runtime spawner used by [`RefreshDriver::spawn_proactive`] is passed
    /// directly to that method (Task 5) rather than stored on `AuthState`; it
    /// is never re-read after the proactive task starts.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        config: ClaudeAiOAuthConfig,
        access_token: Secret<String>,
        refresh_token: Option<Secret<String>>,
        expires_at: SystemTime,
        scopes: Vec<String>,
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
                scopes,
            }),
            refresh_lock: Arc::new(Mutex::new(())),
            proactive_handle: RwLock::new(None),
            http,
            clock,
            bus,
            credentials,
        })
    }

    /// Test-only constructor. Production code uses [`AuthState::new`] (Task 4)
    /// which wires HTTP transport + clock + telemetry + credential manager.
    #[must_use]
    pub fn new_for_test(
        config: ClaudeAiOAuthConfig,
        access_token: Secret<String>,
        refresh_token: Option<Secret<String>>,
        expires_at: SystemTime,
    ) -> Arc<Self> {
        let scopes = config.scopes.clone();
        Arc::new(Self {
            config,
            token: RwLock::new(TokenInfo {
                access_token,
                refresh_token,
                expires_at,
                scopes,
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
    pub async fn proactive_handle(&self) -> Option<lingxi_core::host::BackgroundTaskHandle> {
        self.proactive_handle.read().await.clone()
    }

    /// Cancel the proactive refresh task (if running) and emit
    /// `tengu_oauth_proactive_canceled`. Idempotent — safe to call after the
    /// handle is already cleared.
    ///
    /// `spawner` must be the same `RuntimeSpawner` used by `spawn_proactive`;
    /// if a different one is passed, the handle may not be recognized and
    /// `cancel` returns `RuntimeError::NotFound` which we treat as a no-op.
    pub async fn shutdown(&self, spawner: &dyn lingxi_core::host::RuntimeSpawner) {
        let handle = self.proactive_handle.write().await.take();
        let Some(handle) = handle else {
            // Already shut down — emit no event; matches v3 §16.4 idempotency.
            return;
        };
        if let Err(e) = spawner.cancel(&handle).await {
            tracing::warn!(
                target: "lingxi::anthropic_oauth::shutdown",
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
        token.scopes.clear();
    }

    /// Perform the actual HTTP refresh POST. Returns the parsed response on
    /// 200, or maps non-2xx to an [`OAuthError`]. Called from inside
    /// `refresh_lock`.
    async fn do_refresh_http(
        &self,
        refresh_token: &Secret<String>,
    ) -> Result<RefreshResponse, OAuthError> {
        sdk::refresh_token(
            self.http.as_ref(),
            &self.config,
            refresh_token.expose_secret(),
        )
        .await
        .map_err(|error| match error {
            TokenError::InvalidRefreshToken => OAuthError::RefreshExpired,
            other => OAuthError::TokenExchange(other.to_string()),
        })
    }

    /// Persist the rotated [`TokenInfo`] to the keychain via
    /// [`secret::CredentialManager::store_oauth_tokens`] if a manager is
    /// attached. No-op (returns `Ok`) when no manager is configured (the
    /// in-memory-only test path).
    ///
    /// A refresh response carries no identity, so the user's email / `org_id` are
    /// read back from the previously-persisted session metadata and preserved.
    /// If no prior session blob exists (refresh before any login persisted),
    /// the identity falls back to empty strings — the access / refresh tokens
    /// are still written so the next process start can use them. The persisted
    /// `subscription_type` / `rate_limit_tier` are carried over INSIDE
    /// `store_oauth_tokens` (claude-code `ltu()` merge), so rotation never
    /// drops them.
    async fn persist_to_keychain(&self, info: &TokenInfo) -> Result<(), OAuthError> {
        let Some(cm) = &self.credentials else {
            return Ok(());
        };
        // Preserve the prior identity (email/org) from the stored session blob.
        let (email, org_id) = match cm.get_oauth_tokens().await {
            Ok(Some(prev)) => (prev.email, prev.org_id),
            _ => (String::new(), String::new()),
        };
        let refresh = info
            .refresh_token
            .as_ref()
            .map(|s| s.expose_secret().clone());
        cm.store_oauth_tokens(
            info.access_token.expose_secret(),
            refresh.as_deref(),
            info.expires_at,
            info.scopes.clone(),
            &email,
            &org_id,
        )
        .await
        .map_err(|e| OAuthError::TokenExchange(format!("keychain store: {e}")))?;
        Ok(())
    }
}

/// Test-only transport that must never be called.
struct NullTransport;
#[async_trait]
impl Transport for NullTransport {
    async fn send(
        &self,
        _req: lingxi_llm_client::transport::HttpRequest,
    ) -> Result<lingxi_llm_client::transport::StreamResponse, lingxi_llm_client::protocol::LlmError>
    {
        panic!("NullTransport: test forgot to inject a real transport");
    }
}

/// Null clock — always returns [`SystemTime::UNIX_EPOCH`]. Used by
/// [`AuthState::new_for_test`].
struct NullClock;
impl lingxi_core::host::Clock for NullClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
}

/// Concrete `OAuthRefreshHook` impl: drives reactive + proactive refresh.
///
/// Construction is cheap — actual refresh work happens in `refresh()` (Task 4)
/// and `spawn_proactive()` (Task 5).
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
    pub async fn invalidate(&self, spawner: &dyn lingxi_core::host::RuntimeSpawner) {
        self.state.shutdown(spawner).await;
        self.state.invalidate().await;
    }
}

impl RefreshDriver {
    /// Perform a single-flight OAuth token refresh.
    ///
    /// Inherent method (replaces the former `OAuthRefreshHook` trait impl from
    /// api-client, removed in Plan 3a/3b). All callers within this crate
    /// (`credential_provider.rs`, `proactive_loop`) and tests call this
    /// directly without trait indirection.
    ///
    /// **Single-flight contract (v3 §16.3):** acquires `refresh_lock`, then
    /// double-checks the `prev_token_hash`. If another task already rotated
    /// the token under the lock, returns the current token without making an
    /// HTTP call.
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

        // 5. Build the new `TokenInfo`.
        let now = self.state.clock.now();
        let new_expiry = now + Duration::from_secs(body.expires_in);
        let scopes = body.granted_scopes(&self.state.config);

        let new_access_token_str = body.access_token.clone();
        let new_info = TokenInfo {
            access_token: Secret::new(body.access_token),
            refresh_token: body.refresh_token.map(Secret::new).or(Some(refresh_token)),
            expires_at: new_expiry,
            scopes,
        };

        // 6. Atomic swap. The double-check-after-acquire above guarantees we're
        //    the only writer in flight.
        {
            let mut guard = self.state.token.write().await;
            *guard = new_info;
        }

        // 7. Persist to keychain (best-effort — never fails the refresh on a
        //    keychain write error; the in-memory rotation succeeded).
        if let Err(e) = self
            .state
            .persist_to_keychain(&*self.state.token.read().await)
            .await
        {
            tracing::warn!(
                target: "lingxi::anthropic_oauth::refresh",
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
    lifecycle::emit_refresh_started(bus, "tengu_oauth_refresh_started", trigger).await;
}

async fn emit_refresh_succeeded(
    bus: &Option<Arc<telemetry::AnalyticsBus>>,
    trigger: &str,
    new_expiry_unix: i64,
    duration_ms: u64,
) {
    lifecycle::emit_refresh_succeeded(
        bus,
        "tengu_oauth_refresh_succeeded",
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
    lifecycle::emit_refresh_failed(bus, "tengu_oauth_refresh_failed", trigger, error_kind).await;
}

async fn emit_proactive_canceled(bus: &Option<Arc<telemetry::AnalyticsBus>>, reason: &str) {
    lifecycle::emit_proactive_canceled(bus, "tengu_oauth_proactive_canceled", reason).await;
}

/// Maximum lead time before token expiry that the proactive refresh task wakes.
///
/// Spec §7 line 721. 5 minutes = 300 seconds. Locked byte-for-byte against
/// claude-code's proactive-refresh timer.
pub const PROACTIVE_LEAD_CAP: Duration = Duration::from_secs(5 * 60);

/// Compute the proactive refresh lead for a token with `remaining` lifetime.
///
/// Returns `min(remaining / 2, PROACTIVE_LEAD_CAP)` in whole seconds. Spec §7
/// line 721 — handles short-lived debug tokens (TTL < 5 min) by waking at
/// remaining/2 instead of a fixed 5-minute lead that would never fire.
///
/// Division is integer-floor over whole seconds to match claude-code's TS
/// `Math.floor(remaining / 2)` semantics.
#[must_use]
pub fn proactive_lead(remaining: Duration) -> Duration {
    lifecycle::proactive_lead(remaining, PROACTIVE_LEAD_CAP)
}

impl RefreshDriver {
    /// Spawn the proactive refresh task. Returns once the spawn handle is
    /// stored in `state.proactive_handle`. The task itself loops until canceled
    /// by `AuthState::shutdown`.
    ///
    /// Wake interval is `min(remaining_lifetime / 2, PROACTIVE_LEAD_CAP)`
    /// before expiry (spec §7 line 721).
    pub async fn spawn_proactive(
        state: Arc<AuthState>,
        spawner: Arc<dyn lingxi_core::host::RuntimeSpawner>,
    ) -> Result<(), OAuthError> {
        let task_state = state.clone();
        let task_spawner = spawner.clone();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> =
            Box::pin(async move {
                proactive_loop(task_state, task_spawner).await;
            });
        let handle = spawner
            .spawn("lingxi-oauth-proactive-refresh", fut)
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("spawn failed: {e}")))?;
        *state.proactive_handle.write().await = Some(handle);
        Ok(())
    }
}

/// The proactive task loop. Wakes at `min(remaining/2, 5min)` before expiry,
/// calls `RefreshDriver::refresh`, and reschedules against the new expiry.
async fn proactive_loop(
    state: Arc<AuthState>,
    spawner: Arc<dyn lingxi_core::host::RuntimeSpawner>,
) {
    let driver = RefreshDriver::new(state.clone());
    loop {
        // Read current expiry + token_hash.
        let (expires_at, prev_hash) = {
            let t = state.token.read().await;
            (t.expires_at, t.token_hash())
        };
        let now = state.clock.now();
        let remaining = expires_at.duration_since(now).unwrap_or(Duration::ZERO);
        let lead = proactive_lead(remaining);
        let sleep_for = remaining.checked_sub(lead).unwrap_or(Duration::ZERO);

        emit_refresh_started(&state.bus, "proactive_timer").await;

        // Sleep until the wake instant. `RuntimeSpawner::sleep` lets tests
        // virtualize time.
        spawner.sleep(sleep_for).await;

        // Fire a refresh. If it fails we emit a `_failed` event and back off
        // for 30 seconds; on hard failure (RefreshExpired) we exit the loop —
        // the next API call will surface the auth error to the user.
        match driver.refresh(prev_hash).await {
            Ok(_) => {
                // Success — loop continues against the freshly-rotated token.
                continue;
            }
            Err(OAuthHookError::RefreshFailed(msg))
                if msg == "Session expired. Re-authenticate?" =>
            {
                tracing::error!(
                    target: "lingxi::anthropic_oauth::proactive",
                    "refresh_token expired; exiting proactive loop"
                );
                emit_refresh_failed(&state.bus, "proactive_timer", "refresh_expired").await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "lingxi::anthropic_oauth::proactive",
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
mod wire_and_persist_tests {
    use super::*;
    use crate::auth::anthropic::testsupport::{
        mem_credential_manager, Canned, MemStorage, MockHttp, TestClock,
    };
    use lingxi_core::types::HttpMethod;

    /// Reactive refresh must send a JSON body carrying the `scope` param and
    /// persist the rotated tokens to the attached `CredentialManager`.
    #[tokio::test]
    async fn reactive_refresh_sends_json_scope_and_persists() {
        let resp = r#"{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600,"scope":"read:user write:messages read:projects"}"#;
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp.into(),
            },
        )]);
        let clock = TestClock::new(2_000);
        let storage = MemStorage::new();
        let cm = mem_credential_manager(storage.clone(), clock.clone());
        // Seed a prior session so email/org are preserved across the rotation.
        cm.store_oauth_tokens(
            "OLD_ACCESS",
            Some("OLD_REFRESH"),
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_010),
            vec!["read:user".into()],
            "user@example.com",
            "org-uuid-9",
        )
        .await
        .expect("seed session");

        let cfg = ClaudeAiOAuthConfig::default_with_port(0);
        let state = AuthState::new(
            cfg,
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_010),
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0)
                .scopes,
            http.clone() as Arc<dyn Transport>,
            clock.clone() as Arc<dyn lingxi_core::host::Clock>,
            None,
            Some(cm.clone()),
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();

        let token = driver.refresh(prev).await.expect("refresh ok");
        assert_eq!(token.0.expose_secret(), "NEW_ACCESS");
        assert_eq!(http.call_count(), 1);

        // Wire shape: POST JSON with grant_type=refresh_token + scope.
        let req = http.last_request().expect("request made");
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json"));
        let sent: serde_json::Value =
            serde_json::from_str(req.body.as_deref().unwrap()).expect("json body");
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "OLD_REFRESH");
        assert_eq!(sent["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(
            sent["scope"],
            lingxi_llm_client::auth::oauth::anthropic::CLAUDE_CODE_OAUTH_SCOPES.join(" ")
        );

        // Persisted: rotated tokens written, identity preserved.
        let persisted = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(persisted.access_token.expose_secret(), "NEW_ACCESS");
        assert_eq!(
            persisted
                .refresh_token
                .as_ref()
                .map(|s| s.expose_secret().clone()),
            Some("NEW_REFRESH".to_string())
        );
        assert_eq!(persisted.email, "user@example.com");
        assert_eq!(persisted.org_id, "org-uuid-9");
        assert_eq!(
            persisted.expires_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2_000 + 3_600)
        );
    }

    /// Proactive loop driven by a virtual spawner (sleeps resolve instantly):
    /// the timer fires `refresh`, and a 401 from the endpoint exits the loop.
    /// We assert exactly one HTTP call (the loop terminates on RefreshExpired
    /// rather than spinning).
    #[tokio::test]
    async fn proactive_loop_fires_then_exits_on_401() {
        use crate::auth::anthropic::testsupport::InstantSpawner;

        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = ClaudeAiOAuthConfig::default_with_port(0);
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            // Expires soon so the proactive lead computation yields a short sleep.
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0)
                .scopes,
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

        // Advance the clock to the wake instant so `remaining` collapses and the
        // loop reaches its refresh attempt.
        clock.set(2);

        // Poll until the single HTTP call lands (the loop exits on 401, so it
        // never exceeds one call).
        for _ in 0..100 {
            if http.call_count() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Give the loop a chance to (incorrectly) spin if it didn't exit.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            http.call_count(),
            1,
            "proactive fired once and exited on 401 (no spin)"
        );
    }

    /// A 401 from the token endpoint maps to the locked re-auth error and does
    /// NOT touch the keychain.
    #[tokio::test]
    async fn reactive_refresh_401_maps_to_session_expired() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = ClaudeAiOAuthConfig::default_with_port(0);
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0)
                .scopes,
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

    /// OAUTHREF.3: the refresh POST requests the canonical default scope set
    /// (`config.scopes`), NOT the current token's (possibly narrower) scopes.
    /// Mirrors TS `refreshOAuthToken` defaulting to `CLAUDE_AI_OAUTH_SCOPES`
    /// when no explicit scopes are requested — this is what lets the backend
    /// expand scopes on refresh without forcing a re-login. Here the live token
    /// holds only `read:user`, yet the request must still carry the full
    /// canonical set.
    #[tokio::test]
    async fn refresh_sends_canonical_default_scope_not_current_token_scopes() {
        let resp = r#"{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600,"scope":"read:user write:messages read:projects"}"#;
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp.into(),
            },
        )]);
        let clock = TestClock::new(0);
        let cfg = ClaudeAiOAuthConfig::default_with_port(0);
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0)
                .scopes,
            http.clone() as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        // Simulate a token that currently holds only a narrow subset of scopes.
        // (token_hash is over the access_token only, so this does not perturb
        // the double-check-after-acquire.)
        state.token.write().await.scopes = vec!["read:user".into()];

        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let req = http.last_request().expect("request made");
        let sent: serde_json::Value =
            serde_json::from_str(req.body.as_deref().unwrap()).expect("json body");
        // Canonical default (config.scopes), NOT the narrow "read:user" held.
        assert_eq!(
            sent["scope"],
            lingxi_llm_client::auth::oauth::anthropic::CLAUDE_CODE_OAUTH_SCOPES.join(" ")
        );
    }

    /// OAUTHREF.2: the refresh POST uses a 15s timeout, matching claude-code's
    /// `refreshOAuthToken` `timeout: 15000` (services/oauth/client.ts:168).
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
        let cfg = ClaudeAiOAuthConfig::default_with_port(0);
        let state = AuthState::new(
            cfg,
            Secret::new("ACCESS".into()),
            Some(Secret::new("REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0)
                .scopes,
            http.clone() as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            None,
        );
        let driver = RefreshDriver::new(state.clone());
        let prev = state.token.read().await.token_hash();
        driver.refresh(prev).await.expect("refresh ok");

        let req = http.last_request().expect("request made");
        assert!(req.timeout.is_some_and(
            |timeout| timeout > Duration::from_secs(14) && timeout <= Duration::from_secs(15)
        ));
    }
}
