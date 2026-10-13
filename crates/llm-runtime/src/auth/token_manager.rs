//! One manager for every OAuth session this engine refreshes itself.
//!
//! A provider describes only what differs: how its token endpoint redeems a
//! refresh token, where the session is stored, and when it falls due
//! ([`OAuthSession`]). [`OAuthTokenManager`] owns what must not differ, because
//! every refresh rotates the refresh token and a second refresh with the old
//! one ends the session:
//! - single flight: one refresh at a time, and a caller that saw the old token
//!   gets the rotated one instead of refreshing again;
//! - across processes that share the store, a lock file and a second read of
//!   the stored session under it: a session another process already rotated is
//!   adopted, not refreshed again (claude-code's
//!   `checkAndRefreshOAuthTokenIfNeeded`);
//! - the rotated session is stored before that lock is released;
//! - the proactive task that refreshes before expiry.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, RuntimeSpawner};
use lingxi_core::types::Secret;
use rand::Rng;
use tokio::sync::{Mutex, RwLock};

use crate::auth::lifecycle::{
    self, BearerToken, OAuthHookError, Preflight, RefreshableToken, TokenHash,
};

/// The refresh failure that ends a session: the provider rejected its
/// refresh token, so only a new sign-in helps.
pub(crate) const SESSION_EXPIRED: &str = "Session expired. Re-authenticate?";

/// How often a refresh retries a lock file another process holds, and how long
/// it waits in between (claude-code: five retries, one to two seconds each).
const SHARED_LOCK_RETRIES: u32 = 5;

/// Telemetry and task names one provider reports its refreshes under.
#[derive(Debug)]
pub struct RefreshEvents {
    /// A refresh started.
    pub started: &'static str,
    /// A refresh rotated the session.
    pub succeeded: &'static str,
    /// A refresh failed.
    pub failed: &'static str,
    /// The proactive task was stopped.
    pub proactive_canceled: &'static str,
    /// Name of the proactive background task.
    pub proactive_task: &'static str,
    /// Provider named in log records.
    pub provider: &'static str,
}

/// Held while this process refreshes a session that other processes store too.
pub type SharedRefreshGuard = Box<dyn Any + Send>;

/// Serializes refreshes of one stored session across the processes sharing it.
#[async_trait]
pub trait SharedRefreshLock: Send + Sync {
    /// Take the lock without waiting. `Ok(None)` means another process holds it.
    async fn try_acquire(&self) -> Result<Option<SharedRefreshGuard>, String>;
}

/// The provider half of a session an [`OAuthTokenManager`] keeps fresh.
#[async_trait]
pub trait OAuthSession: Send + Sync + 'static {
    /// The provider's in-memory session.
    type Token: RefreshableToken + Send + Sync + 'static;

    /// Names this provider reports refreshes under.
    fn events(&self) -> &'static RefreshEvents;
    /// The current session.
    fn token(&self) -> &RwLock<Self::Token>;
    /// Held across a whole refresh: request, swap and store.
    fn refresh_lock(&self) -> &Arc<Mutex<()>>;
    /// The proactive task, while one runs.
    fn proactive_task(&self) -> &RwLock<Option<BackgroundTaskHandle>>;
    /// Wall clock that decides expiry.
    fn now(&self) -> SystemTime;
    /// Refresh telemetry.
    fn bus(&self) -> &Option<Arc<telemetry::AnalyticsBus>>;
    /// The lock shared with other processes that store this session, if any.
    fn shared_lock(&self) -> Option<&Arc<dyn SharedRefreshLock>>;

    /// When `token` expires.
    fn expires_at(token: &Self::Token) -> SystemTime;
    /// How long the proactive task waits before it refreshes `token`.
    fn refresh_due_in(token: &Self::Token, now: SystemTime) -> Duration;
    /// Redeem `refresh_token` at the provider for the next session. A rejected
    /// refresh token is `RefreshFailed(SESSION_EXPIRED)`; any other failure is
    /// `ProviderUnreachable`.
    async fn exchange(&self, refresh_token: Secret<String>) -> Result<Self::Token, OAuthHookError>;
    /// Store a rotated session.
    async fn persist(&self, token: &Self::Token) -> Result<(), String>;
    /// The session as currently stored, read past any cache. `None` when no
    /// store is attached or it holds no session.
    async fn stored(&self) -> Option<Self::Token>;
    /// Forget the session in place.
    fn clear(token: &mut Self::Token);
}

/// Keeps one provider's session fresh. Every refresh of that session, reactive
/// or proactive, goes through [`OAuthTokenManager::refresh`].
pub struct OAuthTokenManager<S: OAuthSession> {
    pub(crate) state: Arc<S>,
}

impl<S: OAuthSession> OAuthTokenManager<S> {
    /// Manage `state`.
    #[must_use]
    pub fn new(state: Arc<S>) -> Self {
        Self { state }
    }

    /// Stop proactive refresh and forget the in-memory session.
    pub async fn invalidate(&self, spawner: &dyn RuntimeSpawner) {
        shutdown(&*self.state, spawner).await;
        invalidate(&*self.state).await;
    }

    /// Refresh the session a caller saw as `prev_token_hash`.
    ///
    /// Single flight: holds the refresh lock through the request, the swap and
    /// the store. A caller whose token was already rotated, here or by another
    /// process sharing the store, gets the rotated token without a request.
    pub async fn refresh(&self, prev_token_hash: TokenHash) -> Result<BearerToken, OAuthHookError> {
        let state = &*self.state;
        let events = state.events();
        let _guard = state.refresh_lock().lock().await;
        let refresh_token = match lifecycle::preflight(state.token(), prev_token_hash).await? {
            Preflight::AlreadyRotated(bearer) => return Ok(bearer),
            Preflight::RefreshWith(token) => token,
        };

        let started = std::time::Instant::now();
        lifecycle::emit_refresh_started(state.bus(), events.started, "reactive_401").await;

        let shared = match state.shared_lock() {
            None => None,
            Some(lock) => match acquire_shared(lock.as_ref(), events).await {
                Ok(guard) => {
                    // Read the store again under the lock: another process may
                    // have rotated the session since this one loaded it.
                    if let Some(bearer) = adopt_stored(state, prev_token_hash).await {
                        return Ok(bearer);
                    }
                    guard
                }
                Err(error) => {
                    if let Some(bearer) = adopt_stored(state, prev_token_hash).await {
                        return Ok(bearer);
                    }
                    lifecycle::emit_refresh_failed(
                        state.bus(),
                        events.failed,
                        "reactive_401",
                        "provider_unreachable",
                    )
                    .await;
                    return Err(error);
                }
            },
        };

        let token = match state.exchange(refresh_token).await {
            Ok(token) => token,
            Err(error) => {
                let kind = match error {
                    OAuthHookError::RefreshFailed(_) => "refresh_expired",
                    _ => "provider_unreachable",
                };
                lifecycle::emit_refresh_failed(state.bus(), events.failed, "reactive_401", kind)
                    .await;
                // Another process may have won a race this one lost.
                if state.shared_lock().is_some() {
                    if let Some(bearer) = adopt_stored(state, prev_token_hash).await {
                        return Ok(bearer);
                    }
                }
                return Err(error);
            }
        };

        let new_expiry = S::expires_at(&token);
        let bearer = token.access_token().expose_secret().clone();
        *state.token().write().await = token;
        // Best effort: a failed write leaves the rotation in memory.
        if let Err(error) = state.persist(&*state.token().read().await).await {
            tracing::warn!(
                target: "lingxi::oauth::refresh",
                provider = events.provider,
                error = %error,
                "keychain persistence failed; in-memory token is still rotated",
            );
        }
        drop(shared);

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let new_expiry_unix = i64::try_from(
            new_expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .unwrap_or(i64::MAX);
        lifecycle::emit_refresh_succeeded(
            state.bus(),
            events.succeeded,
            "reactive_401",
            new_expiry_unix,
            duration_ms,
        )
        .await;
        Ok(BearerToken(Secret::new(bearer)))
    }

    /// Spawn the proactive task: it refreshes each session when it falls due,
    /// backs off 30 seconds after a transient failure, and ends once the
    /// provider rejects the refresh token.
    ///
    /// # Errors
    /// Fails when the spawner cannot start the task.
    pub async fn spawn_proactive(
        state: Arc<S>,
        spawner: Arc<dyn RuntimeSpawner>,
    ) -> Result<(), String> {
        let task_state = state.clone();
        let task_spawner = spawner.clone();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> =
            Box::pin(async move {
                proactive_loop(task_state, task_spawner).await;
            });
        let handle = spawner
            .spawn(state.events().proactive_task, fut)
            .await
            .map_err(|e| format!("spawn failed: {e}"))?;
        *state.proactive_task().write().await = Some(handle);
        Ok(())
    }
}

/// Stop the proactive task, if one runs. Idempotent.
///
/// `spawner` must be the one that spawned the task; another reports the
/// handle as unknown, which is treated as already stopped.
pub(crate) async fn shutdown<S: OAuthSession>(state: &S, spawner: &dyn RuntimeSpawner) {
    let Some(handle) = state.proactive_task().write().await.take() else {
        return;
    };
    if let Err(e) = spawner.cancel(&handle).await {
        tracing::warn!(
            target: "lingxi::oauth::shutdown",
            provider = state.events().provider,
            error = ?e,
            task_name = %handle.task_name,
            "proactive task cancel returned error; treating as no-op",
        );
    }
    lifecycle::emit_proactive_canceled(
        state.bus(),
        state.events().proactive_canceled,
        "engine_shutdown",
    )
    .await;
}

/// Forget the in-memory session after its stored copy was removed, so an
/// engine already built stops authenticating with it.
pub(crate) async fn invalidate<S: OAuthSession>(state: &S) {
    S::clear(&mut *state.token().write().await);
}

async fn acquire_shared(
    lock: &dyn SharedRefreshLock,
    events: &RefreshEvents,
) -> Result<Option<SharedRefreshGuard>, OAuthHookError> {
    for attempt in 0..=SHARED_LOCK_RETRIES {
        match lock.try_acquire().await {
            Ok(Some(guard)) => return Ok(Some(guard)),
            Ok(None) if attempt < SHARED_LOCK_RETRIES => {
                let jitter = rand::rng().random_range(0..1_000);
                tokio::time::sleep(Duration::from_millis(1_000 + jitter)).await;
            }
            Ok(None) => break,
            // A lock that cannot even be created must not strand the session:
            // refresh as a single process would.
            Err(error) => {
                tracing::warn!(
                    target: "lingxi::oauth::refresh",
                    provider = events.provider,
                    error = %error,
                    "shared refresh lock unavailable; refreshing without it",
                );
                return Ok(None);
            }
        }
    }
    Err(OAuthHookError::ProviderUnreachable(
        "another process is refreshing this session".into(),
    ))
}

/// Adopt the stored session when it is not the one the caller saw and is
/// still valid: another process rotated it.
async fn adopt_stored<S: OAuthSession>(state: &S, previous: TokenHash) -> Option<BearerToken> {
    let stored = state.stored().await?;
    let access = stored.access_token().expose_secret();
    if access.is_empty()
        || lifecycle::token_hash(stored.access_token()) == previous
        || S::expires_at(&stored) <= state.now()
    {
        return None;
    }
    let bearer = BearerToken(Secret::new(access.clone()));
    *state.token().write().await = stored;
    Some(bearer)
}

async fn proactive_loop<S: OAuthSession>(state: Arc<S>, spawner: Arc<dyn RuntimeSpawner>) {
    let events = state.events();
    let manager = OAuthTokenManager::new(state.clone());
    loop {
        let (due_in, prev_hash) = {
            let token = state.token().read().await;
            (
                S::refresh_due_in(&token, state.now()),
                lifecycle::token_hash(token.access_token()),
            )
        };

        lifecycle::emit_refresh_started(state.bus(), events.started, "proactive_timer").await;

        spawner.sleep(due_in).await;

        match manager.refresh(prev_hash).await {
            Ok(_) => continue,
            Err(OAuthHookError::RefreshFailed(msg)) if msg == SESSION_EXPIRED => {
                tracing::error!(
                    target: "lingxi::oauth::proactive",
                    provider = events.provider,
                    "refresh_token expired; exiting proactive loop"
                );
                lifecycle::emit_refresh_failed(
                    state.bus(),
                    events.failed,
                    "proactive_timer",
                    "refresh_expired",
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "lingxi::oauth::proactive",
                    provider = events.provider,
                    error = ?e,
                    "transient refresh failure; backing off 30s",
                );
                lifecycle::emit_refresh_failed(
                    state.bus(),
                    events.failed,
                    "proactive_timer",
                    "provider_unreachable",
                )
                .await;
                spawner.sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::openai::refresh::{AuthState, RefreshDriver};
    use crate::auth::openai::testsupport::{
        mem_credential_manager, Canned, MemStorage, MockHttp, TestClock,
    };
    use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
    use lingxi_llm_client::Transport;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A lock file that is free, or held by another process for good.
    struct TestLock {
        busy: AtomicBool,
        taken: AtomicUsize,
        held: Arc<AtomicBool>,
    }

    struct Held(Arc<AtomicBool>);
    impl Drop for Held {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    impl TestLock {
        fn new(busy: bool) -> Arc<Self> {
            Arc::new(Self {
                busy: AtomicBool::new(busy),
                taken: AtomicUsize::new(0),
                held: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    #[async_trait]
    impl SharedRefreshLock for TestLock {
        async fn try_acquire(&self) -> Result<Option<SharedRefreshGuard>, String> {
            if self.busy.load(Ordering::SeqCst) {
                return Ok(None);
            }
            self.taken.fetch_add(1, Ordering::SeqCst);
            self.held.store(true, Ordering::SeqCst);
            Ok(Some(Box::new(Held(self.held.clone()))))
        }
    }

    const ROTATED: &str =
        r#"{"access_token":"NEW_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600}"#;

    /// An expired in-memory session whose store this process shares.
    async fn shared_session(
        http: Arc<MockHttp>,
        lock: Option<Arc<TestLock>>,
    ) -> (Arc<AuthState>, Arc<secret::CredentialManager>) {
        let clock = TestClock::new(1_000);
        let credentials = mem_credential_manager(MemStorage::new(), clock.clone());
        credentials
            .store_openai_oauth_tokens(
                "OLD_ACCESS",
                Some("OLD_REFRESH"),
                SystemTime::UNIX_EPOCH + Duration::from_secs(900),
                vec![],
                Some("acc"),
                false,
                None,
            )
            .await
            .unwrap();
        let state = AuthState::new(
            OpenAiOAuthConfig::default(),
            Secret::new("OLD_ACCESS".into()),
            Some(Secret::new("OLD_REFRESH".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(900),
            Some("acc".into()),
            false,
            None,
            http as Arc<dyn Transport>,
            clock as Arc<dyn lingxi_core::host::Clock>,
            None,
            Some(credentials.clone()),
        );
        if let Some(lock) = lock {
            state.share_refresh_across_processes(lock);
        }
        (state, credentials)
    }

    /// Another process sharing the store rotates the session.
    async fn rotate_elsewhere(credentials: &secret::CredentialManager) {
        credentials
            .store_openai_oauth_tokens(
                "OTHER_ACCESS",
                Some("OTHER_REFRESH"),
                SystemTime::UNIX_EPOCH + Duration::from_secs(5_000),
                vec![],
                Some("acc"),
                false,
                None,
            )
            .await
            .unwrap();
    }

    async fn access(state: &AuthState) -> String {
        state
            .token
            .read()
            .await
            .access_token
            .expose_secret()
            .clone()
    }

    #[tokio::test]
    async fn a_session_another_process_rotated_is_adopted_without_a_request() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: ROTATED.into(),
            },
        )]);
        let lock = TestLock::new(false);
        let (state, credentials) = shared_session(http.clone(), Some(lock.clone())).await;
        rotate_elsewhere(&credentials).await;

        let prev = state.token.read().await.token_hash();
        let bearer = RefreshDriver::new(state.clone())
            .refresh(prev)
            .await
            .unwrap();

        assert_eq!(bearer.0.expose_secret(), "OTHER_ACCESS");
        assert_eq!(access(&state).await, "OTHER_ACCESS");
        assert_eq!(
            state
                .token
                .read()
                .await
                .refresh_token
                .as_ref()
                .unwrap()
                .expose_secret(),
            "OTHER_REFRESH"
        );
        assert_eq!(
            http.call_count(),
            0,
            "the old refresh token must not be sent"
        );
        assert_eq!(lock.taken.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refresh_under_the_lock_stores_the_rotation_before_letting_go() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: ROTATED.into(),
            },
        )]);
        let lock = TestLock::new(false);
        let (state, credentials) = shared_session(http.clone(), Some(lock.clone())).await;

        let prev = state.token.read().await.token_hash();
        let bearer = RefreshDriver::new(state.clone())
            .refresh(prev)
            .await
            .unwrap();

        assert_eq!(bearer.0.expose_secret(), "NEW_ACCESS");
        assert_eq!(http.call_count(), 1);
        let stored = credentials
            .get_openai_oauth_tokens()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.access_token.expose_secret(), "NEW_ACCESS");
        assert_eq!(stored.refresh_token.unwrap().expose_secret(), "NEW_REFRESH");
        assert!(
            !lock.held.load(Ordering::SeqCst),
            "the lock is released afterwards"
        );
    }

    #[tokio::test]
    async fn a_rejected_refresh_adopts_the_rotation_that_won_the_race() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let lock = TestLock::new(false);
        let (state, credentials) = shared_session(http.clone(), Some(lock)).await;
        let prev = state.token.read().await.token_hash();
        let driver = RefreshDriver::new(state.clone());

        // Nothing rotated elsewhere: the session really is over.
        assert!(matches!(
            driver.refresh(prev).await,
            Err(OAuthHookError::RefreshFailed(msg)) if msg == SESSION_EXPIRED
        ));

        // The provider rejected the old token because another process had just
        // redeemed it; that process's session is the one to use.
        rotate_elsewhere(&credentials).await;
        let bearer = driver.refresh(prev).await.unwrap();
        assert_eq!(bearer.0.expose_secret(), "OTHER_ACCESS");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lock_held_elsewhere_yields_the_session_its_holder_stored() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: ROTATED.into(),
            },
        )]);
        let lock = TestLock::new(true);
        let (state, credentials) = shared_session(http.clone(), Some(lock.clone())).await;
        let prev = state.token.read().await.token_hash();
        let driver = RefreshDriver::new(state.clone());

        assert!(matches!(
            driver.refresh(prev).await,
            Err(OAuthHookError::ProviderUnreachable(_))
        ));
        assert_eq!(
            http.call_count(),
            0,
            "never refresh while another process holds the lock"
        );

        rotate_elsewhere(&credentials).await;
        let bearer = driver.refresh(prev).await.unwrap();
        assert_eq!(bearer.0.expose_secret(), "OTHER_ACCESS");
        assert_eq!(http.call_count(), 0);
    }

    #[tokio::test]
    async fn a_session_no_other_process_shares_is_refreshed_without_reading_the_store() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: ROTATED.into(),
            },
        )]);
        let (state, credentials) = shared_session(http.clone(), None).await;
        rotate_elsewhere(&credentials).await;

        let prev = state.token.read().await.token_hash();
        let bearer = RefreshDriver::new(state).refresh(prev).await.unwrap();

        assert_eq!(bearer.0.expose_secret(), "NEW_ACCESS");
        assert_eq!(http.call_count(), 1);
    }
}
