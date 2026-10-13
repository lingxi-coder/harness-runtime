//! A ChatGPT session that a host delivers to a running engine.
//!
//! A host that runs several engines lets exactly one of them refresh its
//! ChatGPT session, because every refresh rotates the refresh token. Moving
//! the session through this slot hands it to another engine without stopping
//! the one that held it: [`OpenAiOAuthSlot::install`] starts serving and
//! proactively refreshing a session, and [`OpenAiOAuthSlot::release`] lets a
//! rotation in flight finish, and reach the host's observer, before it stops
//! refreshing and forgets the session.

use std::fmt;
use std::sync::{Arc, OnceLock};

use lingxi_core::host::{Clock, RuntimeSpawner};
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use lingxi_llm_client::Transport;
use tokio::sync::{Mutex, RwLock};

use crate::auth::openai::credential_provider::OpenAiOAuthCredentialProvider;
use crate::auth::openai::login::{init_refresh_driver, OAuthError};
use crate::auth::openai::refresh::{AuthState, RefreshDriver};
use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

/// What the serving engine supplies to refresh and persist the session.
pub struct OpenAiOAuthSlotBinding {
    /// OAuth client configuration.
    pub config: OpenAiOAuthConfig,
    /// Provider transport used for token refresh.
    pub http: Arc<dyn Transport>,
    /// Clock that decides expiry.
    pub clock: Arc<dyn Clock>,
    /// Refresh telemetry.
    pub bus: Option<Arc<telemetry::AnalyticsBus>>,
    /// Persists rotations; the host's observer receives them from here.
    pub credentials: Option<Arc<secret::CredentialManager>>,
    /// Runs the proactive refresh task. Release cancels the task through this
    /// same spawner, which is the only one that can find it.
    pub spawner: Arc<dyn RuntimeSpawner>,
}

struct InstalledSession {
    state: Arc<AuthState>,
    provider: OpenAiOAuthCredentialProvider,
}

/// A ChatGPT session slot that its host fills and empties while the engine
/// runs. An empty slot answers like an absent credential.
#[derive(Default)]
pub struct OpenAiOAuthSlot {
    binding: OnceLock<OpenAiOAuthSlotBinding>,
    session: RwLock<Option<Arc<InstalledSession>>>,
    /// Serializes install and release.
    lifecycle: Mutex<()>,
}

impl fmt::Debug for OpenAiOAuthSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiOAuthSlot").finish_non_exhaustive()
    }
}

impl OpenAiOAuthSlot {
    /// An empty slot, served once an engine binds it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the serving engine. A slot serves one engine, so a later binding
    /// is ignored.
    pub fn bind(&self, binding: OpenAiOAuthSlotBinding) {
        let _ = self.binding.set(binding);
    }

    /// Serve `tokens` from now on, replacing any session the slot held.
    ///
    /// # Errors
    /// Fails when no engine serves the slot or the refresh task cannot start.
    pub async fn install(
        &self,
        tokens: secret::credential::OpenAiOAuthTokens,
    ) -> Result<(), OAuthError> {
        let binding = self.binding.get().ok_or_else(|| {
            OAuthError::TokenExchange("no engine serves this ChatGPT session".into())
        })?;
        let _lifecycle = self.lifecycle.lock().await;
        self.release_installed(binding).await;
        let state = init_refresh_driver(
            binding.config.clone(),
            tokens.access_token,
            tokens.refresh_token,
            tokens.expires_at,
            tokens.account_id,
            tokens.fedramp,
            tokens.email,
            binding.http.clone(),
            binding.clock.clone(),
            binding.bus.clone(),
            binding.credentials.clone(),
            binding.spawner.clone(),
        )
        .await?;
        let provider =
            OpenAiOAuthCredentialProvider::new(Arc::new(RefreshDriver::new(state.clone())));
        *self.session.write().await = Some(Arc::new(InstalledSession { state, provider }));
        Ok(())
    }

    /// Stop serving and refreshing the session. A rotation already in flight
    /// finishes first, so its tokens have reached the host when this returns.
    pub async fn release(&self) {
        let Some(binding) = self.binding.get() else {
            return;
        };
        let _lifecycle = self.lifecycle.lock().await;
        self.release_installed(binding).await;
    }

    /// Whether a session is installed.
    pub async fn is_active(&self) -> bool {
        self.session.read().await.is_some()
    }

    async fn release_installed(&self, binding: &OpenAiOAuthSlotBinding) {
        let Some(session) = self.session.write().await.take() else {
            return;
        };
        let _rotation = session.state.refresh_lock.lock().await;
        session.state.shutdown(binding.spawner.as_ref()).await;
        session.state.invalidate().await;
    }

    async fn installed(&self) -> Option<Arc<InstalledSession>> {
        self.session.read().await.clone()
    }
}

impl CredentialProvider for OpenAiOAuthSlot {
    fn source<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
        Box::pin(async move {
            match self.installed().await {
                Some(session) => session.provider.source(scope).await,
                None => Ok(crate::CredentialSource::None),
            }
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            match self.installed().await {
                Some(session) => session.provider.load(scope).await,
                None => Err(LlmError::Authentication {
                    message: String::new(),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::openai::testsupport::{
        mem_credential_manager, MemStorage, MockHttp, TestClock,
    };
    use async_trait::async_trait;
    use lingxi_core::host::{BackgroundTaskHandle, RuntimeError};
    use lingxi_core::types::Secret;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    /// Parks the proactive loop so a test observes install and release only.
    #[derive(Default)]
    struct ParkedSpawner {
        next_id: AtomicU64,
        canceled: std::sync::Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl RuntimeSpawner for ParkedSpawner {
        async fn spawn(
            &self,
            name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            Ok(BackgroundTaskHandle {
                task_name: name.to_string(),
                task_id: self.next_id.fetch_add(1, Ordering::SeqCst) + 1,
            })
        }
        async fn sleep(&self, _duration: Duration) {
            std::future::pending::<()>().await;
        }
        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            self.canceled.lock().unwrap().push(handle.task_id);
            Ok(())
        }
    }

    fn tokens(access: &str) -> secret::credential::OpenAiOAuthTokens {
        secret::credential::OpenAiOAuthTokens {
            access_token: Secret::new(access.into()),
            refresh_token: Some(Secret::new(format!("{access}-refresh"))),
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(9_999_999),
            scopes: vec![],
            account_id: Some("acc_XYZ".into()),
            fedramp: false,
            email: None,
        }
    }

    fn bound_slot(spawner: Arc<ParkedSpawner>) -> OpenAiOAuthSlot {
        let clock = TestClock::new(100);
        let slot = OpenAiOAuthSlot::new();
        slot.bind(OpenAiOAuthSlotBinding {
            config: OpenAiOAuthConfig::default(),
            http: MockHttp::new(vec![]) as Arc<dyn Transport>,
            clock: clock.clone() as Arc<dyn Clock>,
            bus: None,
            credentials: Some(mem_credential_manager(
                MemStorage::new(),
                clock as Arc<dyn Clock>,
            )),
            spawner,
        });
        slot
    }

    fn scope() -> CredentialScope {
        CredentialScope::new(crate::ProviderId::OpenAI, "chatgpt")
    }

    async fn access_token(slot: &OpenAiOAuthSlot) -> Option<String> {
        match slot.load(&scope()).await {
            Ok(Credential::ChatGptOAuth { access_token, .. }) => Some(access_token),
            _ => None,
        }
    }

    #[tokio::test]
    async fn an_empty_slot_answers_like_an_absent_credential() {
        let slot = bound_slot(Arc::new(ParkedSpawner::default()));
        assert_eq!(
            slot.source(&scope()).await.unwrap(),
            crate::CredentialSource::None
        );
        assert!(matches!(
            slot.load(&scope()).await,
            Err(LlmError::Authentication { message }) if message.is_empty()
        ));
        assert!(!slot.is_active().await);
    }

    #[tokio::test]
    async fn install_serves_the_session_and_release_stops_its_refresh_task() {
        let spawner = Arc::new(ParkedSpawner::default());
        let slot = bound_slot(spawner.clone());
        slot.install(tokens("first")).await.unwrap();
        assert!(slot.is_active().await);
        assert_eq!(access_token(&slot).await.as_deref(), Some("first"));

        slot.release().await;
        assert!(!slot.is_active().await);
        assert_eq!(access_token(&slot).await, None);
        assert_eq!(*spawner.canceled.lock().unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn install_replaces_the_previous_session_and_its_refresh_task() {
        let spawner = Arc::new(ParkedSpawner::default());
        let slot = bound_slot(spawner.clone());
        slot.install(tokens("first")).await.unwrap();
        slot.install(tokens("second")).await.unwrap();
        assert_eq!(access_token(&slot).await.as_deref(), Some("second"));
        assert_eq!(*spawner.canceled.lock().unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn release_waits_for_a_rotation_in_flight() {
        let spawner = Arc::new(ParkedSpawner::default());
        let slot = Arc::new(bound_slot(spawner));
        slot.install(tokens("first")).await.unwrap();
        let installed = slot.installed().await.unwrap();
        let rotation = installed.state.refresh_lock.clone().lock_owned().await;
        let releasing = tokio::spawn({
            let slot = slot.clone();
            async move { slot.release().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !releasing.is_finished(),
            "release must wait for the rotation"
        );
        assert_eq!(
            installed
                .state
                .token
                .read()
                .await
                .access_token
                .expose_secret(),
            "first"
        );
        drop(rotation);
        releasing.await.unwrap();
        assert!(installed
            .state
            .token
            .read()
            .await
            .access_token
            .expose_secret()
            .is_empty());
    }

    #[tokio::test]
    async fn an_unbound_slot_refuses_a_session() {
        let slot = OpenAiOAuthSlot::new();
        assert!(slot.install(tokens("first")).await.is_err());
        slot.release().await;
        assert!(!slot.is_active().await);
    }
}
