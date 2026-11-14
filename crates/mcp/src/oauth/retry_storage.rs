//! Short, cancellation-safe credential commits for automatic authentication retry.
//! Network authentication runs against a private snapshot; it cannot clear or
//! overwrite a grant published by another owner while the request was pending.

use super::{session, OAuthError, StoredTokens, MCP_OAUTH_SERVICE};
use lingxi_core::host::{
    McpTransportSpec, SecureStorage, SecureStorageBackend, SecureStorageError,
};
use lingxi_core::types::SecureStorageData;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

struct MutationGuard {
    _file: Option<lingxi_core::host::RootedFileLock>,
    _memory: Option<tokio::sync::OwnedMutexGuard<()>>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OAuthCredentialIdentity {
    bearer: String,
    grant: Option<String>,
}

impl OAuthCredentialIdentity {
    pub(crate) fn from_spec(spec: &McpTransportSpec, grant: Option<String>) -> Option<Self> {
        let headers = match spec {
            McpTransportSpec::Http { headers, .. } | McpTransportSpec::Sse { headers, .. } => {
                headers
            }
            _ => return None,
        };
        let value = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))?
            .1;
        Some(Self {
            bearer: format!("{:x}", Sha256::digest(value.as_bytes())),
            grant,
        })
    }

    pub(crate) fn matches(&self, stored: &StoredTokens) -> bool {
        let grant = stored
            .refresh_token
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(super::discovery_cache_refresh_grant_token)
            .map(|value| crate::discovery_cache::fingerprint(&value));
        self.bearer
            == format!(
                "{:x}",
                Sha256::digest(format!("Bearer {}", stored.access_token).as_bytes())
            )
            && self.grant == grant
    }
}

fn error(value: impl std::fmt::Display) -> SecureStorageError {
    SecureStorageError::Io(format!("MCP OAuth credential commit: {value}"))
}

/// A distinct lock from the long-lived refresh lease: publishers never hold
/// this lock during HTTP, browser interaction, or another refresh-lock wait.
fn process_local_mutation(storage: &Arc<dyn SecureStorage>) -> bool {
    matches!(
        storage.account_ownership_backend(),
        SecureStorageBackend::MemorySession
            | SecureStorageBackend::AndroidKeystore
            | SecureStorageBackend::IosKeychain
    )
}

async fn mutation_lock(
    storage: &Arc<dyn SecureStorage>,
    key: &str,
) -> Result<MutationGuard, SecureStorageError> {
    mutation_lock_with_home(storage, key, || {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    })
    .await
}

async fn mutation_lock_with_home(
    storage: &Arc<dyn SecureStorage>,
    key: &str,
    resolve_home: impl FnOnce() -> std::path::PathBuf,
) -> Result<MutationGuard, SecureStorageError> {
    // Native mobile stores have no desktop HOME contract. Keep their existing
    // host-local publisher scope; a shared extension domain must be supplied by
    // its native host rather than guessed from HOME or a temporary directory.
    let home = if process_local_mutation(storage) {
        std::path::PathBuf::new()
    } else {
        resolve_home()
    };
    mutation_lock_in_home(storage, key, home).await
}

async fn mutation_lock_in_home(
    storage: &Arc<dyn SecureStorage>,
    key: &str,
    home: std::path::PathBuf,
) -> Result<MutationGuard, SecureStorageError> {
    if process_local_mutation(storage) {
        // Memory and native mobile publishers have no desktop filesystem
        // requirement. The same-key lock still coordinates independent owners.
        static LOCKS: OnceLock<Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>> =
            OnceLock::new();
        let lock = {
            let mut locks = LOCKS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks.retain(|_, lock| lock.strong_count() != 0);
            let lock = locks
                .get(key)
                .and_then(std::sync::Weak::upgrade)
                .unwrap_or_default();
            locks.insert(key.to_owned(), Arc::downgrade(&lock));
            lock
        };
        return Ok(MutationGuard {
            _file: None,
            _memory: Some(lock.lock_owned().await),
        });
    }
    // Credential commits share one stable host namespace. A caller's
    // configurable refresh-lock directory must not split the CAS from other
    // publishers of this same secure-storage credential. HOME already exists;
    // create the private subdirectories relative to its pinned handle instead
    // of passing an uninitialized credential directory as the rooted-FS root.
    let relative = std::path::PathBuf::from(branding::DOT_DIR)
        .join("mcp-credential-commits")
        .join(format!("{:x}.lock", Sha256::digest(key.as_bytes())));
    tokio::task::spawn_blocking(move || {
        let identity = lingxi_core::host::rooted_fs::root_identity(&home).map_err(error)?;
        let file = lingxi_core::host::lock_exclusive_pinned(
            &home,
            &relative,
            0o700,
            0o600,
            Some(&identity),
        )
        .map_err(error)?;
        Ok(MutationGuard {
            _file: Some(file),
            _memory: None,
        })
    })
    .await
    .map_err(error)?
}

/// Every token/session publisher uses this short transaction. A cancelled
/// caller leaves the owned task holding the lock until its backend write ends.
pub(super) async fn write(
    storage: &Arc<dyn SecureStorage>,
    service: &str,
    key: &str,
    value: Option<SecureStorageData>,
) -> Result<(), SecureStorageError> {
    let storage = storage.clone();
    let service = service.to_owned();
    let key = key.to_owned();
    tokio::spawn(async move {
        let _lock = mutation_lock(&storage, &key).await?;
        match value {
            Some(value) => storage.store(&service, &key, value).await,
            None => storage.delete(&service, &key).await,
        }
    })
    .await
    .map_err(error)?
}

pub(crate) struct OAuthRetryStorage {
    base: Arc<dyn SecureStorage>,
    key: String,
    original: BTreeMap<String, Option<SecureStorageData>>,
    staged: Mutex<BTreeMap<String, Option<SecureStorageData>>>,
}

impl OAuthRetryStorage {
    pub(crate) async fn capture(
        base: Arc<dyn SecureStorage>,
        key: String,
    ) -> Result<Arc<Self>, OAuthError> {
        let _lock = mutation_lock(&base, &key)
            .await
            .map_err(|e| OAuthError::Token(e.to_string()))?;
        let mut original = BTreeMap::new();
        for service in [MCP_OAUTH_SERVICE, session::SERVICE] {
            let value = base
                .retrieve_uncached(service, &key)
                .await
                .map_err(|e| OAuthError::Token(e.to_string()))?;
            original.insert(service.to_owned(), value);
        }
        Ok(Arc::new(Self {
            base,
            key,
            original,
            staged: Mutex::new(BTreeMap::new()),
        }))
    }

    pub(crate) fn tokens(&self) -> Result<Option<StoredTokens>, OAuthError> {
        Self::decode(
            self.original
                .get(MCP_OAUTH_SERVICE)
                .and_then(Option::as_ref),
        )
    }

    fn decode(data: Option<&SecureStorageData>) -> Result<Option<StoredTokens>, OAuthError> {
        data.map(|data| {
            serde_json::from_slice(data.expose_secret_bytes())
                .map_err(|e| OAuthError::Token(format!("decode stored tokens: {e}")))
        })
        .transpose()
    }

    /// Check real authority before another network/browser step. Snapshot
    /// reads stay frozen so a failed A flow cannot refresh the new B grant.
    pub(crate) async fn winner(&self) -> Result<Option<Option<StoredTokens>>, OAuthError> {
        let _lock = mutation_lock(&self.base, &self.key)
            .await
            .map_err(|e| OAuthError::Token(e.to_string()))?;
        let mut changed = false;
        let mut tokens = None;
        for (service, original) in &self.original {
            let current = self
                .base
                .retrieve_uncached(service, &self.key)
                .await
                .map_err(|e| OAuthError::Token(e.to_string()))?;
            changed |= original
                .as_ref()
                .map(SecureStorageData::expose_secret_bytes)
                != current.as_ref().map(SecureStorageData::expose_secret_bytes);
            if service == MCP_OAUTH_SERVICE {
                tokens = Self::decode(current.as_ref())?;
            }
        }
        Ok(changed.then_some(tokens))
    }

    /// Return the current winner if either authority changed. No staged side
    /// effect escapes on failure, deletion, or supersession.
    pub(crate) async fn finish(
        self: &Arc<Self>,
        commit: bool,
    ) -> Result<(Option<StoredTokens>, bool), OAuthError> {
        let transaction = self.clone();
        tokio::spawn(async move {
            let _lock = mutation_lock(&transaction.base, &transaction.key)
                .await
                .map_err(|e| OAuthError::Token(e.to_string()))?;
            let mut current = BTreeMap::new();
            for service in transaction.original.keys() {
                current.insert(
                    service.clone(),
                    transaction
                        .base
                        .retrieve_uncached(service, &transaction.key)
                        .await
                        .map_err(|e| OAuthError::Token(e.to_string()))?,
                );
            }
            let unchanged = transaction.original.iter().all(|(service, old)| {
                old.as_ref().map(SecureStorageData::expose_secret_bytes)
                    == current[service]
                        .as_ref()
                        .map(SecureStorageData::expose_secret_bytes)
            });
            if !commit || !unchanged {
                return Self::decode(current[MCP_OAUTH_SERVICE].as_ref())
                    .map(|tokens| (tokens, false));
            }
            let staged = transaction
                .staged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // Session metadata belongs to the same accepted token publication.
            // Persist it first, then publish the token as the commit point.
            for service in [session::SERVICE, MCP_OAUTH_SERVICE] {
                if let Some(value) = staged.get(service) {
                    let result = match value {
                        Some(data) => {
                            transaction
                                .base
                                .store(service, &transaction.key, data.clone())
                                .await
                        }
                        None => transaction.base.delete(service, &transaction.key).await,
                    };
                    if let Err(failure) = result {
                        // Backends may fail after publication. Restore both
                        // authorities, including the failing write, before
                        // releasing this cancellation-safe commit owner.
                        let mut compensation = Vec::new();
                        for restore in [session::SERVICE, MCP_OAUTH_SERVICE] {
                            let result = match transaction.original[restore].as_ref() {
                                Some(data) => {
                                    transaction
                                        .base
                                        .store(restore, &transaction.key, data.clone())
                                        .await
                                }
                                None => transaction.base.delete(restore, &transaction.key).await,
                            };
                            if let Err(error) = result {
                                compensation.push(error.to_string());
                            }
                        }
                        return Err(OAuthError::Token(if compensation.is_empty() {
                            format!("OAuth recovery commit failed and was rolled back: {failure}")
                        } else {
                            format!(
                                "OAuth recovery commit failed: {failure}; compensation failed: {}",
                                compensation.join("; ")
                            )
                        }));
                    }
                }
            }
            let tokens = staged
                .get(MCP_OAUTH_SERVICE)
                .unwrap_or(&transaction.original[MCP_OAUTH_SERVICE]);
            Self::decode(tokens.as_ref()).map(|tokens| (tokens, true))
        })
        .await
        .map_err(|e| OAuthError::Token(e.to_string()))?
    }
}

#[async_trait::async_trait]
impl SecureStorage for OAuthRetryStorage {
    async fn store(
        &self,
        service: &str,
        key: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        if key != self.key || !self.original.contains_key(service) {
            return Err(error("retry escaped its credential scope"));
        }
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(service.to_owned(), Some(data));
        Ok(())
    }
    async fn retrieve(
        &self,
        service: &str,
        key: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        if key != self.key || !self.original.contains_key(service) {
            return Err(error("retry escaped its credential scope"));
        }
        Ok(self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(service)
            .cloned()
            .unwrap_or_else(|| self.original[service].clone()))
    }
    async fn retrieve_uncached(
        &self,
        service: &str,
        key: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.retrieve(service, key).await
    }
    async fn delete(&self, service: &str, key: &str) -> Result<(), SecureStorageError> {
        if key != self.key || !self.original.contains_key(service) {
            return Err(error("retry escaped its credential scope"));
        }
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(service.to_owned(), None);
        Ok(())
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        Ok(self
            .retrieve(service, &self.key)
            .await?
            .is_some()
            .then(|| self.key.clone())
            .into_iter()
            .collect())
    }
    fn is_encrypted(&self) -> bool {
        self.base.is_encrypted()
    }
    fn backend(&self) -> SecureStorageBackend {
        self.base.backend()
    }
    fn account_ownership_backend(&self) -> SecureStorageBackend {
        self.base.account_ownership_backend()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::secure_storage::InMemorySecureStorage;
    use lingxi_core::host::Clock;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct FaultStore {
        base: InMemorySecureStorage,
        ownership: Mutex<Option<SecureStorageBackend>>,
        fallback: AtomicBool,
        fail: AtomicUsize,
        after_publication: AtomicBool,
        block: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    fn code(service: &str) -> usize {
        if service == session::SERVICE {
            1
        } else {
            2
        }
    }
    #[async_trait::async_trait]
    impl SecureStorage for FaultStore {
        async fn store(
            &self,
            service: &str,
            key: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            let fail = self
                .fail
                .compare_exchange(code(service), 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            if fail && !self.after_publication.load(Ordering::SeqCst) {
                return Err(error("fixture before-publication write failure"));
            }
            self.base.store(service, key, data).await?;
            if self
                .block
                .compare_exchange(code(service), 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if fail {
                return Err(error("fixture after-publication write failure"));
            }
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            key: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            self.base.retrieve(service, key).await
        }
        async fn delete(&self, service: &str, key: &str) -> Result<(), SecureStorageError> {
            self.base.delete(service, key).await
        }
        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            self.base.list(service).await
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            if self.fallback.load(Ordering::SeqCst) {
                SecureStorageBackend::MemorySession
            } else {
                self.account_ownership_backend()
            }
        }
        fn account_ownership_backend(&self) -> SecureStorageBackend {
            self.ownership
                .lock()
                .unwrap()
                .unwrap_or(SecureStorageBackend::MemorySession)
        }
    }
    struct ClockNow;
    impl Clock for ClockNow {
        fn now(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH
        }
    }
    fn token(value: &str) -> StoredTokens {
        StoredTokens {
            access_token: value.into(),
            refresh_token: Some(format!("refresh-{value}")),
            expires_at_millis: super::super::OAuthExpiry::Undefined,
            client_id: None,
            client_secret: None,
            step_up_scope: None,
        }
    }
    async fn seed(storage: &Arc<dyn SecureStorage>, value: &str) {
        let clock: Arc<dyn Clock> = Arc::new(ClockNow);
        let state = session::OAuthSessionState {
            client_registration: Some(value.into()),
            ..Default::default()
        };
        session::save(storage, &clock, "fixture", &state)
            .await
            .unwrap();
        super::super::store_tokens(storage, &clock, "fixture", &token(value))
            .await
            .unwrap();
    }
    async fn staged(storage: Arc<dyn SecureStorage>) -> Arc<OAuthRetryStorage> {
        let transaction = OAuthRetryStorage::capture(storage, "fixture".into())
            .await
            .unwrap();
        let overlay: Arc<dyn SecureStorage> = transaction.clone();
        seed(&overlay, "b").await;
        transaction
    }

    #[tokio::test]
    async fn first_native_or_fallback_publication_creates_private_lock_parents_safely() {
        for fallback in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let fault = Arc::new(FaultStore::default());
            *fault.ownership.lock().unwrap() = Some(SecureStorageBackend::MacOsKeychain);
            fault.fallback.store(fallback, Ordering::SeqCst);
            let storage: Arc<dyn SecureStorage> = fault.clone();
            assert!(!home.path().join(branding::DOT_DIR).exists());
            let guard =
                mutation_lock_in_home(&storage, "native-first-write", home.path().to_path_buf())
                    .await
                    .unwrap();
            assert!(
                guard._file.is_some(),
                "stable native ownership also fences its fallback"
            );
            let relative = std::path::PathBuf::from(branding::DOT_DIR)
                .join("mcp-credential-commits")
                .join(format!("{:x}.lock", Sha256::digest(b"native-first-write")));
            assert!(home.path().join(&relative).is_file());
            fault.fallback.store(!fallback, Ordering::SeqCst);
            assert!(
                lingxi_core::host::rooted_fs::try_lock_exclusive(
                    home.path(),
                    &relative,
                    0o700,
                    0o600
                )
                .is_err(),
                "a backend transition cannot split the credential lock"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(home.path().join(branding::DOT_DIR))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                assert_eq!(
                    std::fs::metadata(home.path().join(&relative))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            drop(guard);
            let _next =
                mutation_lock_in_home(&storage, "native-first-write", home.path().to_path_buf())
                    .await
                    .unwrap();
        }
    }

    #[tokio::test]
    async fn memory_android_and_ios_publishers_have_no_home_or_private_filesystem_dependency() {
        for backend in [
            SecureStorageBackend::MemorySession,
            SecureStorageBackend::AndroidKeystore,
            SecureStorageBackend::IosKeychain,
        ] {
            for fallback in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let absent_home = directory.path().join("home-must-not-be-created");
                let fault = Arc::new(FaultStore::default());
                *fault.ownership.lock().unwrap() = Some(backend);
                fault.fallback.store(fallback, Ordering::SeqCst);
                let storage: Arc<dyn SecureStorage> = fault;
                let guard = mutation_lock_with_home(&storage, "mobile", || {
                    panic!("native mobile must not resolve desktop HOME")
                })
                .await
                .unwrap();
                assert!(guard._memory.is_some() && guard._file.is_none());
                assert!(!absent_home.exists());
                drop(guard);
                seed(&storage, "a").await;
                let transaction = staged(storage.clone()).await;
                let (current, committed) = transaction.finish(true).await.unwrap();
                assert!(committed);
                assert_eq!(current.unwrap(), token("b"));
                assert!(!absent_home.exists());
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_first_write_refuses_symlinked_private_parent_without_touching_target() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), home.path().join(branding::DOT_DIR)).unwrap();
        let fault = Arc::new(FaultStore::default());
        *fault.ownership.lock().unwrap() = Some(SecureStorageBackend::MacOsKeychain);
        let storage: Arc<dyn SecureStorage> = fault;
        assert!(
            mutation_lock_in_home(&storage, "native", home.path().to_path_buf())
                .await
                .is_err()
        );
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn failed_session_or_token_commit_compensates_before_and_after_publication() {
        for service in [session::SERVICE, MCP_OAUTH_SERVICE] {
            for after_publication in [false, true] {
                let fault = Arc::new(FaultStore::default());
                let storage: Arc<dyn SecureStorage> = fault.clone();
                seed(&storage, "a").await;
                let transaction = staged(storage.clone()).await;
                fault
                    .after_publication
                    .store(after_publication, Ordering::SeqCst);
                fault.fail.store(code(service), Ordering::SeqCst);
                let failure = transaction.finish(true).await.unwrap_err().to_string();
                assert!(failure.contains("rolled back"), "{failure}");
                assert_eq!(
                    super::super::load_tokens(&storage, "fixture")
                        .await
                        .unwrap()
                        .unwrap(),
                    token("a")
                );
                assert_eq!(
                    session::load(&storage, "fixture")
                        .await
                        .unwrap()
                        .client_registration
                        .as_deref(),
                    Some("a")
                );
            }
        }
    }

    #[tokio::test]
    async fn cancelled_commit_keeps_ownership_until_completion_then_allows_newer_publication() {
        let fault = Arc::new(FaultStore::default());
        let storage: Arc<dyn SecureStorage> = fault.clone();
        seed(&storage, "a").await;
        let transaction = staged(storage.clone()).await;
        fault.block.store(code(session::SERVICE), Ordering::SeqCst);
        let commit = tokio::spawn(async move { transaction.finish(true).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), fault.entered.notified())
            .await
            .unwrap();
        commit.abort();
        let mut newer = Box::pin(seed(&storage, "c"));
        assert!(futures_util::poll!(newer.as_mut()).is_pending());
        fault.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), newer)
            .await
            .unwrap();
        assert_eq!(
            super::super::load_tokens(&storage, "fixture")
                .await
                .unwrap()
                .unwrap(),
            token("c")
        );
        assert_eq!(
            session::load(&storage, "fixture")
                .await
                .unwrap()
                .client_registration
                .as_deref(),
            Some("c")
        );
    }

    #[tokio::test]
    async fn superseding_authority_wins_without_any_staged_side_effects() {
        let fault = Arc::new(FaultStore::default());
        let storage: Arc<dyn SecureStorage> = fault;
        seed(&storage, "a").await;
        let transaction = staged(storage.clone()).await;
        seed(&storage, "c").await;
        assert_eq!(
            transaction.winner().await.unwrap().unwrap().unwrap(),
            token("c")
        );
        let (current, committed) = transaction.finish(true).await.unwrap();
        assert!(!committed);
        assert_eq!(current.unwrap(), token("c"));
        assert_eq!(
            session::load(&storage, "fixture")
                .await
                .unwrap()
                .client_registration
                .as_deref(),
            Some("c")
        );
    }
}
