//! A ChatGPT session whose host keeps it fresh.
//!
//! A host that runs several engines on one ChatGPT account refreshes the
//! session itself, because every refresh rotates the refresh token and only one
//! refresher may hold it. The engines receive only access tokens: the host
//! serves each new one through [`HostSession::serve`], and an engine whose
//! token expired asks for the next one through its [`HostRefreshRequester`]
//! and waits for it. Any number of engines can use the account at once.

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use lingxi_core::host::Clock;
use lingxi_core::types::Secret;
use tokio::sync::watch;

use crate::auth::lifecycle::{token_hash, TokenHash};
use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

/// How long an engine waits for its host to serve a renewed token.
const RENEWAL_WAIT: Duration = Duration::from_secs(30);

/// An access token the host served.
pub struct HostToken {
    /// Bearer access token.
    pub access_token: Secret<String>,
    /// When the access token expires.
    pub expires_at: SystemTime,
    /// `ChatGPT` workspace/account id.
    pub account_id: Option<String>,
    /// `FedRAMP` account flag.
    pub fedramp: bool,
}

// `Secret` is deliberately not `Clone`; copy it by re-wrapping.
impl Clone for HostToken {
    fn clone(&self) -> Self {
        Self {
            access_token: Secret::new(self.access_token.expose_secret().clone()),
            expires_at: self.expires_at,
            account_id: self.account_id.clone(),
            fedramp: self.fedramp,
        }
    }
}

impl fmt::Debug for HostToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostToken")
            .field("access_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .field("fedramp", &self.fedramp)
            .finish()
    }
}

/// Asks the host for a renewed access token.
pub trait HostRefreshRequester: Send + Sync {
    /// The host should serve a token newer than the one it served last. The
    /// request is fire-and-forget; the answer arrives through
    /// [`HostSession::serve`].
    fn request_refresh(&self);
}

/// The ChatGPT session an engine serves on its host's behalf. Empty until the
/// host serves a token; an empty session answers like an absent credential.
pub struct HostSession {
    current: watch::Sender<Option<HostToken>>,
    requester: OnceLock<Arc<dyn HostRefreshRequester>>,
    /// The token a renewal was last requested for, so concurrent requests
    /// that found the same expired token ask the host once.
    requested_for: Mutex<Option<TokenHash>>,
    clock: Arc<dyn Clock>,
    wait: Duration,
}

impl fmt::Debug for HostSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSession").finish_non_exhaustive()
    }
}

impl HostSession {
    /// An empty session that judges expiry by `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            current: watch::Sender::new(None),
            requester: OnceLock::new(),
            requested_for: Mutex::new(None),
            clock,
            wait: RENEWAL_WAIT,
        }
    }

    #[cfg(test)]
    fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// Ask `requester` for renewals from now on. The first one wins.
    pub fn request_refreshes_through(&self, requester: Arc<dyn HostRefreshRequester>) {
        let _ = self.requester.set(requester);
    }

    /// Serve `token` from now on, waking every request waiting for it.
    pub fn serve(&self, token: HostToken) {
        self.current.send_replace(Some(token));
    }

    /// Stop serving the session: the host signed out.
    pub fn clear(&self) {
        self.current.send_replace(None);
    }

    /// Whether a token is being served.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.current.borrow().is_some()
    }

    fn valid(&self, token: &HostToken) -> bool {
        token.expires_at > self.clock.now()
    }

    /// A token other than the one hashed `rejected`, once the host serves one.
    async fn renewed(&self, rejected: TokenHash) -> Result<HostToken, LlmError> {
        let mut updates = self.current.subscribe();
        let fresh = |token: &Option<HostToken>| {
            token
                .as_ref()
                .filter(|token| token_hash(&token.access_token) != rejected && self.valid(token))
                .cloned()
        };
        if let Some(token) = fresh(&updates.borrow_and_update()) {
            return Ok(token);
        }
        let ask = {
            let mut requested = self
                .requested_for
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ask = *requested != Some(rejected);
            *requested = Some(rejected);
            ask
        };
        if ask {
            if let Some(requester) = self.requester.get() {
                requester.request_refresh();
            }
        }
        let waited = tokio::time::timeout(self.wait, async {
            loop {
                if updates.changed().await.is_err() {
                    return None;
                }
                let token = updates.borrow_and_update().clone();
                match token {
                    // Signed out while waiting.
                    None => return None,
                    some => {
                        if let Some(token) = fresh(&some) {
                            return Some(token);
                        }
                    }
                }
            }
        })
        .await;
        // A later expiry of this same token asks again.
        if let Ok(mut requested) = self.requested_for.lock() {
            if *requested == Some(rejected) {
                *requested = None;
            }
        }
        match waited {
            Ok(Some(token)) => Ok(token),
            _ => Err(LlmError::Authentication {
                message: String::new(),
            }),
        }
    }
}

fn credential(token: HostToken) -> Credential {
    Credential::ChatGptOAuth {
        access_token: token.access_token.expose_secret().clone(),
        account_id: token.account_id,
        fedramp: token.fedramp,
    }
}

impl CredentialProvider for HostSession {
    fn source<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
        Box::pin(async move {
            Ok(if self.is_active() {
                crate::CredentialSource::OAuth
            } else {
                crate::CredentialSource::None
            })
        })
    }

    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let current = self.current.borrow().clone();
            match current {
                None => Err(LlmError::Authentication {
                    message: String::new(),
                }),
                Some(token) if self.valid(&token) => Ok(credential(token)),
                Some(token) => self
                    .renewed(token_hash(&token.access_token))
                    .await
                    .map(credential),
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        _scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            let Credential::ChatGptOAuth { access_token, .. } = rejected else {
                return Ok(None);
            };
            let rejected = token_hash(&Secret::new(access_token.clone()));
            self.renewed(rejected)
                .await
                .map(|token| Some(credential(token)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::openai::testsupport::TestClock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingRequester(AtomicUsize);
    impl HostRefreshRequester for CountingRequester {
        fn request_refresh(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn token(access: &str, expires: u64) -> HostToken {
        HostToken {
            access_token: Secret::new(access.into()),
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(expires),
            account_id: Some("acc".into()),
            fedramp: false,
        }
    }

    fn scope() -> CredentialScope {
        CredentialScope::new(crate::ProviderId::OpenAI, "chatgpt")
    }

    fn access(credential: &Credential) -> &str {
        match credential {
            Credential::ChatGptOAuth { access_token, .. } => access_token,
            other => panic!("wrong credential: {other:?}"),
        }
    }

    fn session(now: u64) -> (Arc<HostSession>, Arc<CountingRequester>) {
        let session = Arc::new(HostSession::new(TestClock::new(now)));
        let requester = Arc::new(CountingRequester(AtomicUsize::new(0)));
        session.request_refreshes_through(requester.clone());
        (session, requester)
    }

    #[tokio::test]
    async fn an_empty_session_answers_like_an_absent_credential() {
        let (session, requester) = session(100);
        assert_eq!(
            session.source(&scope()).await.unwrap(),
            crate::CredentialSource::None
        );
        assert!(matches!(
            session.load(&scope()).await,
            Err(LlmError::Authentication { message }) if message.is_empty()
        ));
        assert_eq!(requester.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_valid_token_is_served_without_asking_the_host() {
        let (session, requester) = session(100);
        session.serve(token("first", 1_000));
        assert_eq!(access(&session.load(&scope()).await.unwrap()), "first");
        assert_eq!(
            session.source(&scope()).await.unwrap(),
            crate::CredentialSource::OAuth
        );
        assert_eq!(requester.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_expired_token_asks_the_host_once_and_waits_for_the_next() {
        let (session, requester) = session(2_000);
        session.serve(token("old", 1_000));
        let first = tokio::spawn({
            let session = session.clone();
            async move { session.load(&scope()).await }
        });
        let second = tokio::spawn({
            let session = session.clone();
            async move { session.load(&scope()).await }
        });
        while requester.0.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        session.serve(token("new", 9_000));
        assert_eq!(access(&first.await.unwrap().unwrap()), "new");
        assert_eq!(access(&second.await.unwrap().unwrap()), "new");
        assert_eq!(requester.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_rejected_token_is_replaced_by_one_the_host_already_served() {
        let (session, requester) = session(100);
        session.serve(token("new", 9_000));
        let rejected = Credential::ChatGptOAuth {
            access_token: "old".into(),
            account_id: None,
            fedramp: false,
        };
        let renewed = session.refresh(&scope(), &rejected).await.unwrap().unwrap();
        assert_eq!(access(&renewed), "new");
        assert_eq!(requester.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_never_answers_fails_the_request() {
        let session =
            Arc::new(HostSession::new(TestClock::new(2_000)).with_wait(Duration::from_secs(5)));
        session.serve(token("old", 1_000));
        assert!(matches!(
            session.load(&scope()).await,
            Err(LlmError::Authentication { .. })
        ));
    }

    #[tokio::test]
    async fn signing_out_while_waiting_fails_the_request() {
        let (session, requester) = session(2_000);
        session.serve(token("old", 1_000));
        let waiting = tokio::spawn({
            let session = session.clone();
            async move { session.load(&scope()).await }
        });
        while requester.0.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        session.clear();
        assert!(waiting.await.unwrap().is_err());
        assert!(!session.is_active());
    }
}
