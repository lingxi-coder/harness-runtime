//! Platform-default [`SecureStorage`] factory.
//!
//! On macOS, requires the bundled credential broker app and does not fall back
//! automatically. Unsigned/mispackaged persistent clients fail closed rather
//! than writing secrets to plaintext or fallback memory. The explicitly
//! isolated packaged bridge policy remains process-local by design and has no
//! persistent broker authority.
//!
//! On Linux, tries [`super::linux::LinuxSecretStorage`] (the `libsecret`
//! `secret-tool` CLI) first, falling back to plaintext on init or runtime
//! availability errors — the same shape as the macOS arm.
//!
//! On any other OS, returns plaintext directly with the documented warning.

use async_trait::async_trait;
use lingxi_core::host::{
    CredentialStoragePolicy, InMemorySecureStorage, SecureStorage, SecureStorageBackend,
    SecureStorageError,
};
use lingxi_core::types::SecureStorageData;
#[cfg(test)]
use lingxi_core::types::{SecretKindDto, SecureStorageMetadata};
use platform_common::secure_storage::RuntimeFallbackStorage;
#[cfg(test)]
use sha2::{Digest, Sha256};

use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::time::SystemTime;
use tokio::sync::OnceCell;

struct DeferredPlainTextStorage {
    base_dir: PathBuf,
    inner: OnceCell<Arc<super::plaintext::PlainTextSecureStorage>>,
}

impl DeferredPlainTextStorage {
    fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            inner: OnceCell::new(),
        }
    }

    async fn storage(
        &self,
    ) -> Result<&Arc<super::plaintext::PlainTextSecureStorage>, SecureStorageError> {
        self.inner
            .get_or_try_init(|| async {
                Ok(Arc::new(
                    super::plaintext::PlainTextSecureStorage::new(self.base_dir.clone()).await?,
                ))
            })
            .await
    }

    async fn storage_if_present(
        &self,
    ) -> Result<Option<&Arc<super::plaintext::PlainTextSecureStorage>>, SecureStorageError> {
        if !tokio::fs::try_exists(&self.base_dir)
            .await
            .map_err(|error| SecureStorageError::Io(error.to_string()))?
        {
            return Ok(None);
        }
        self.storage().await.map(Some)
    }
}

#[async_trait]
impl SecureStorage for DeferredPlainTextStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        self.storage().await?.store(service, account, data).await
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let Some(storage) = self.storage_if_present().await? else {
            return Ok(None);
        };
        storage.retrieve(service, account).await
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        let Some(storage) = self.storage_if_present().await? else {
            return Ok(());
        };
        storage.delete(service, account).await
    }

    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        let Some(storage) = self.storage_if_present().await? else {
            return Ok(Vec::new());
        };
        storage.list(service).await
    }

    fn is_encrypted(&self) -> bool {
        false
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::PlainText
    }
}

struct UnavailableNativeStorage {
    backend: SecureStorageBackend,
    error: SecureStorageError,
}

#[async_trait]
impl SecureStorage for UnavailableNativeStorage {
    async fn store(
        &self,
        _service: &str,
        _account: &str,
        _data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        Err(self.error.clone())
    }

    async fn retrieve(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        Err(self.error.clone())
    }

    async fn delete(&self, _service: &str, _account: &str) -> Result<(), SecureStorageError> {
        Err(self.error.clone())
    }

    async fn list(&self, _service: &str) -> Result<Vec<String>, SecureStorageError> {
        Err(self.error.clone())
    }

    fn is_encrypted(&self) -> bool {
        true
    }

    fn backend(&self) -> SecureStorageBackend {
        self.backend
    }
}

fn native_or_fallback_storage(
    primary: Result<Arc<dyn SecureStorage>, SecureStorageError>,
    native_backend: SecureStorageBackend,
    fallback: Arc<dyn SecureStorage>,
    lock_directory: PathBuf,
) -> Arc<dyn SecureStorage> {
    let (primary, unavailable) = match primary {
        Ok(primary) => (primary, false),
        Err(error) => (
            Arc::new(UnavailableNativeStorage {
                backend: native_backend,
                error,
            }) as Arc<dyn SecureStorage>,
            true,
        ),
    };
    let storage = RuntimeFallbackStorage::new(primary, fallback, lock_directory);
    Arc::new(if unavailable {
        storage.with_fallback_active()
    } else {
        storage
    })
}

/// Build the production fallback wrapper over isolated synthetic backends.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn runtime_fallback_storage_for_test(
    primary: Result<Arc<dyn SecureStorage>, SecureStorageError>,
    native_backend: SecureStorageBackend,
    fallback: Arc<dyn SecureStorage>,
    lock_directory: PathBuf,
) -> Arc<dyn SecureStorage> {
    native_or_fallback_storage(primary, native_backend, fallback, lock_directory)
}

fn fallback_lock_directory(plaintext_path: &Path) -> PathBuf {
    let mut name = plaintext_path
        .file_name()
        .map_or_else(|| OsString::from("credentials"), OsString::from);
    name.push(".locks");
    plaintext_path.with_file_name(name)
}

fn fallback_directory(plaintext_path: &Path) -> PathBuf {
    let mut name = plaintext_path
        .file_name()
        .map_or_else(|| OsString::from("credentials"), OsString::from);
    name.push(".d");
    plaintext_path.with_file_name(name)
}

/// Build the file-backed store directly, skipping every native keychain.
///
/// Exists so a host can be ISOLATED from the machine's real credentials. The
/// native backends are keyed by OS user, not by `config_dir`, so a process that
/// merely points `config_dir` at a temp directory still reads whatever the
/// developer's login keychain holds — which makes "does this install have a
/// credential?" answer differently on a logged-in machine than on a clean one.
/// Callers that need a deterministic answer (tests, sandboxed boots) use this.
///
/// # Errors
/// Propagates [`SecureStorageError`] when the plaintext store cannot be opened.
pub async fn plaintext_secure_storage(
    plaintext_path: PathBuf,
) -> Result<Arc<dyn SecureStorage>, SecureStorageError> {
    Ok(Arc::new(
        super::plaintext::PlainTextSecureStorage::new(plaintext_path).await?,
    ))
}

/// Return the shared [`SecureStorage`] used by CLI, TUI, and Desktop.
///
/// `user` is the native keychain account name. `config_dir` is the `LingXi`
/// configuration directory and `plaintext_path` is its existing OAuth JSON
/// path. On macOS the bundled credential broker is mandatory; only the
/// explicit [`CredentialStoragePolicy::PlainTextFixture`] test path may bypass
/// it. Linux keeps the existing lazy fallback behavior.
pub async fn secure_storage_for_policy(
    user: String,
    config_dir: PathBuf,
    plaintext_path: PathBuf,
    policy: CredentialStoragePolicy,
) -> Result<Arc<dyn SecureStorage>, SecureStorageError> {
    if policy == CredentialStoragePolicy::PlainTextFixture {
        return plaintext_secure_storage(plaintext_path).await;
    }
    #[cfg(target_os = "macos")]
    {
        if policy == CredentialStoragePolicy::NativeOrMemory {
            // Packaged bridge-server uses this explicit isolation policy. Its
            // active provider credential arrives once over stdin and remains
            // process-local; the sidecar never receives broker authority.
            return Ok(Arc::new(InMemorySecureStorage::new()));
        }
        let default_dir = default_lingxi_dir();
        super::macos::MacOsKeychainStorage::new(
            user.clone(),
            config_dir.clone(),
            default_dir,
            String::new(),
        )
        .map(|keychain| Arc::new(keychain) as Arc<dyn SecureStorage>)
    }
    #[cfg(not(target_os = "macos"))]
    let fallback: Arc<dyn SecureStorage> = match policy {
        CredentialStoragePolicy::NativePreferred => Arc::new(DeferredPlainTextStorage::new(
            fallback_directory(&plaintext_path),
        )),
        CredentialStoragePolicy::NativeOrMemory => Arc::new(InMemorySecureStorage::new()),
        CredentialStoragePolicy::PlainTextFixture => unreachable!("handled above"),
    };
    #[cfg(target_os = "linux")]
    {
        // Realizes claude-code's `// TODO: add libsecret support for Linux`:
        // try the `libsecret` `secret-tool` backend first, falling back to
        // plaintext (with the documented warning) when it cannot initialise —
        // the same try-keychain-then-plaintext shape as the macOS arm.
        let default_dir = default_lingxi_dir();
        let primary = super::linux::LinuxSecretStorage::new(
            user.clone(),
            config_dir.clone(),
            default_dir,
            String::new(),
        )
        .map(|storage| Arc::new(storage) as Arc<dyn SecureStorage>);
        if let Err(error) = &primary {
            tracing::warn!(
                target: "lingxi::secure_storage",
                %error,
                policy = ?policy,
                "Warning: native credential storage unavailable during init."
            );
        }
        // Each per-OS block returns; on Linux this one happens to be last.
        #[allow(clippy::needless_return)]
        return Ok(native_or_fallback_storage(
            primary,
            SecureStorageBackend::LinuxLibsecret,
            fallback,
            fallback_lock_directory(&plaintext_path),
        ));
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // No native backend on this OS — fall back to plaintext with the
        // documented warning. (Windows/other; the Linux + macOS arms above
        // only warn when their backend genuinely fails to initialise.)
        tracing::warn!(
            target: "lingxi::secure_storage",
            policy = ?policy,
            "Warning: no native credential storage backend available on this target."
        );
        let _ = &user;
        let _ = &config_dir;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(fallback)
    }
}

/// Build the shared POSIX credential store with the production
/// `NativePreferred` fallback policy.
pub async fn secure_storage_for_platform(
    user: String,
    config_dir: PathBuf,
    plaintext_path: PathBuf,
) -> Result<Arc<dyn SecureStorage>, SecureStorageError> {
    secure_storage_for_policy(
        user,
        config_dir,
        plaintext_path,
        CredentialStoragePolicy::NativePreferred,
    )
    .await
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn default_lingxi_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(branding::DOT_DIR)
    } else {
        PathBuf::from("/").join(branding::DOT_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    struct UnavailableStorage;

    #[async_trait]
    impl SecureStorage for UnavailableStorage {
        async fn store(
            &self,
            _service: &str,
            _account: &str,
            _data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            Err(SecureStorageError::BackendUnavailable(
                "native store locked".to_string(),
            ))
        }

        async fn retrieve(
            &self,
            _service: &str,
            _account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            Err(SecureStorageError::BackendUnavailable(
                "native store locked".to_string(),
            ))
        }

        async fn delete(&self, _service: &str, _account: &str) -> Result<(), SecureStorageError> {
            Err(SecureStorageError::BackendUnavailable(
                "native store locked".to_string(),
            ))
        }

        async fn list(&self, _service: &str) -> Result<Vec<String>, SecureStorageError> {
            Err(SecureStorageError::BackendUnavailable(
                "native store locked".to_string(),
            ))
        }

        fn is_encrypted(&self) -> bool {
            true
        }

        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::MacOsKeychain
        }
    }

    struct MemoryStorage {
        entries: Mutex<std::collections::BTreeMap<(String, String), SecureStorageData>>,
        unavailable: AtomicBool,
        fail_delete: AtomicBool,
        encrypted: bool,
        backend: SecureStorageBackend,
    }

    impl MemoryStorage {
        fn new(encrypted: bool) -> Self {
            Self {
                entries: Mutex::new(std::collections::BTreeMap::new()),
                unavailable: AtomicBool::new(false),
                fail_delete: AtomicBool::new(false),
                encrypted,
                backend: if encrypted {
                    SecureStorageBackend::MacOsKeychain
                } else {
                    SecureStorageBackend::PlainText
                },
            }
        }

        fn with_backend(mut self, backend: SecureStorageBackend) -> Self {
            self.backend = backend;
            self
        }

        fn set_unavailable(&self, unavailable: bool) {
            self.unavailable.store(unavailable, Ordering::Release);
        }

        fn set_fail_delete(&self, fail_delete: bool) {
            self.fail_delete.store(fail_delete, Ordering::Release);
        }

        fn check_available(&self) -> Result<(), SecureStorageError> {
            if self.unavailable.load(Ordering::Acquire) {
                Err(SecureStorageError::BackendUnavailable(
                    "native store locked".to_string(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl SecureStorage for MemoryStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            self.check_available()?;
            self.entries
                .lock()
                .await
                .insert((service.to_string(), account.to_string()), data);
            Ok(())
        }

        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            self.check_available()?;
            Ok(self
                .entries
                .lock()
                .await
                .get(&(service.to_string(), account.to_string()))
                .cloned())
        }

        async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
            self.check_available()?;
            if self.fail_delete.load(Ordering::Acquire) {
                return Err(SecureStorageError::Io(
                    "fallback cleanup denied".to_string(),
                ));
            }
            self.entries
                .lock()
                .await
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }

        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            self.check_available()?;
            Ok(self
                .entries
                .lock()
                .await
                .keys()
                .filter_map(|(stored_service, account)| {
                    (stored_service == service).then_some(account.clone())
                })
                .collect())
        }

        fn is_encrypted(&self) -> bool {
            self.encrypted
        }

        fn backend(&self) -> SecureStorageBackend {
            self.backend
        }
    }

    fn test_payload(value: &[u8]) -> SecureStorageData {
        SecureStorageData::new(
            value.to_vec(),
            SecureStorageMetadata {
                created_at: SystemTime::UNIX_EPOCH,
                last_accessed: None,
                kind: SecretKindDto("generic_api_key".to_string()),
            },
        )
    }

    struct GatedMigrationStorage {
        inner: MemoryStorage,
        pause_old: AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl GatedMigrationStorage {
        fn new() -> Self {
            Self {
                inner: MemoryStorage::new(true),
                pause_old: AtomicBool::new(true),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait]
    impl SecureStorage for GatedMigrationStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            if data.expose_secret_bytes() == b"old-fallback"
                && self.pause_old.swap(false, Ordering::AcqRel)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.store(service, account, data).await
        }

        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            self.inner.retrieve(service, account).await
        }

        async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
            self.inner.delete(service, account).await
        }

        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            self.inner.list(service).await
        }

        fn is_encrypted(&self) -> bool {
            true
        }

        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::LinuxLibsecret
        }
    }

    async fn assert_migration_serializes_later_mutation(delete: bool, cancel_read: bool) {
        use lingxi_core::host::rooted_fs;
        let directory = tempdir().unwrap();
        let lock_directory = directory.path().join("credential-locks");
        let primary = Arc::new(GatedMigrationStorage::new());
        let fallback = Arc::new(MemoryStorage::new(false));
        fallback
            .store("fixture", "key", test_payload(b"old-fallback"))
            .await
            .unwrap();
        let reader = Arc::new(RuntimeFallbackStorage::new(
            primary.clone(),
            fallback.clone(),
            lock_directory.clone(),
        ));
        // Independent owners share the on-disk lock, not an Arc<Mutex>.
        let writer = RuntimeFallbackStorage::new(primary.clone(), fallback, lock_directory.clone());
        let reading = {
            let reader = reader.clone();
            tokio::spawn(async move { reader.retrieve("fixture", "key").await })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            primary.entered.notified(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::metadata(&lock_directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let mut hash = Sha256::new();
        for component in ["fixture", "key"] {
            hash.update((component.len() as u64).to_be_bytes());
            hash.update(component.as_bytes());
        }
        let lock_path = PathBuf::from(format!("{:x}.lock", hash.finalize()));
        assert_eq!(
            std::fs::metadata(lock_directory.join(&lock_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        if cancel_read {
            reading.abort();
        }

        let mut later = Box::pin(async {
            if delete {
                writer.delete("fixture", "key").await
            } else {
                writer
                    .store("fixture", "key", test_payload(b"replacement"))
                    .await
            }
        });
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(later.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        // Dropping another queued caller cannot release the migration's lock.
        let mut queued = Box::pin(reader.retrieve("fixture", "key"));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(queued.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(queued);
        assert!(matches!(
            rooted_fs::try_lock_exclusive(&lock_directory, &lock_path, 0o700, 0o600),
            Err(rooted_fs::FsError::AlreadyExists(_))
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            writer.store("fixture", "unrelated", test_payload(b"other-value")),
        )
        .await
        .unwrap()
        .unwrap();

        primary.release.notify_one();
        let read_result = reading.await;
        if cancel_read {
            assert!(read_result.unwrap_err().is_cancelled());
        } else {
            assert_eq!(
                read_result.unwrap().unwrap().unwrap().expose_secret_bytes(),
                b"old-fallback"
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), later)
            .await
            .unwrap()
            .unwrap();
        let current = writer.retrieve("fixture", "key").await.unwrap();
        if delete {
            assert!(current.is_none());
            assert!(primary.retrieve("fixture", "key").await.unwrap().is_none());
        } else {
            assert_eq!(current.unwrap().expose_secret_bytes(), b"replacement");
            assert_eq!(
                primary
                    .retrieve("fixture", "key")
                    .await
                    .unwrap()
                    .unwrap()
                    .expose_secret_bytes(),
                b"replacement"
            );
        }
    }

    #[tokio::test]
    async fn fallback_migration_cannot_overwrite_an_independent_handles_replacement() {
        assert_migration_serializes_later_mutation(false, false).await;
    }

    #[tokio::test]
    async fn fallback_migration_cannot_resurrect_an_independent_handles_deletion() {
        assert_migration_serializes_later_mutation(true, false).await;
    }

    #[tokio::test]
    async fn cancelled_fallback_migration_retains_its_lock_after_a_waiter_cancels() {
        for delete in [false, true] {
            assert_migration_serializes_later_mutation(delete, true).await;
        }
    }

    #[tokio::test]
    async fn factory_returns_storage_handle() {
        let dir = tempdir().expect("tempdir");
        let plain = dir.path().join("creds-base");
        let result = secure_storage_for_policy(
            "test".into(),
            dir.path().to_path_buf(),
            plain,
            CredentialStoragePolicy::PlainTextFixture,
        )
        .await;
        let storage = result.expect("factory");
        let _ = storage.is_encrypted();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn packaged_bridge_policy_is_explicitly_process_local() {
        let dir = tempdir().expect("tempdir");
        let storage = secure_storage_for_policy(
            "test".into(),
            dir.path().to_path_buf(),
            dir.path().join("creds-base"),
            CredentialStoragePolicy::NativeOrMemory,
        )
        .await
        .expect("isolated bridge storage");
        assert_eq!(storage.backend(), SecureStorageBackend::MemorySession);
        assert_eq!(
            storage.account_ownership_backend(),
            SecureStorageBackend::MemorySession
        );
    }

    #[tokio::test]
    async fn account_ownership_stays_native_during_fallback_recovery_and_reopen() {
        for memory_only in [false, true] {
            let root = tempdir().unwrap();
            let primary = Arc::new(
                MemoryStorage::new(true).with_backend(SecureStorageBackend::LinuxLibsecret),
            );
            let fallback: Arc<dyn SecureStorage> = if memory_only {
                Arc::new(InMemorySecureStorage::new())
            } else {
                Arc::new(DeferredPlainTextStorage::new(root.path().join("fallback")))
            };
            let storage = RuntimeFallbackStorage::new(
                primary.clone(),
                fallback.clone(),
                root.path().join("locks"),
            );
            assert_eq!(
                storage.account_ownership_backend(),
                SecureStorageBackend::LinuxLibsecret
            );
            primary.set_unavailable(true);
            storage
                .store("fixture", "key", test_payload(b"synthetic-key"))
                .await
                .unwrap();
            assert_eq!(storage.backend(), fallback.backend());
            assert_eq!(
                storage.account_ownership_backend(),
                SecureStorageBackend::LinuxLibsecret
            );
            let reopened =
                RuntimeFallbackStorage::new(primary.clone(), fallback, root.path().join("locks"));
            assert_eq!(
                reopened.account_ownership_backend(),
                SecureStorageBackend::LinuxLibsecret
            );
            primary.set_unavailable(false);
            assert!(reopened.retrieve("fixture", "key").await.unwrap().is_some());
            assert_eq!(reopened.backend(), SecureStorageBackend::LinuxLibsecret);
            assert_eq!(
                reopened.account_ownership_backend(),
                SecureStorageBackend::LinuxLibsecret
            );
            assert!(!storage.contains("unrelated", "absent").await.unwrap());
            assert_eq!(storage.backend(), SecureStorageBackend::LinuxLibsecret);
            assert_eq!(
                storage.account_ownership_backend(),
                SecureStorageBackend::LinuxLibsecret
            );
        }
    }

    #[tokio::test]
    async fn initialization_fallback_keeps_native_ownership_without_cross_policy_secret_access() {
        let root = tempdir().unwrap();
        let plaintext = root.path().join("fallback");
        let missing = || {
            Err(SecureStorageError::BackendUnavailable(
                "synthetic native executable missing".into(),
            ))
        };
        let storage = native_or_fallback_storage(
            missing(),
            SecureStorageBackend::LinuxLibsecret,
            Arc::new(DeferredPlainTextStorage::new(plaintext.clone())),
            root.path().join("locks"),
        );
        assert_eq!(storage.backend(), SecureStorageBackend::PlainText);
        assert_eq!(
            storage.account_ownership_backend(),
            SecureStorageBackend::LinuxLibsecret
        );
        storage
            .store("fixture", "key", test_payload(b"synthetic-persistent-key"))
            .await
            .unwrap();

        let isolated = native_or_fallback_storage(
            missing(),
            SecureStorageBackend::LinuxLibsecret,
            Arc::new(InMemorySecureStorage::new()),
            root.path().join("locks"),
        );
        assert_eq!(isolated.backend(), SecureStorageBackend::MemorySession);
        assert_eq!(
            isolated.account_ownership_backend(),
            SecureStorageBackend::LinuxLibsecret
        );
        assert!(matches!(isolated.retrieve("fixture", "key").await,
            Err(SecureStorageError::BackendUnavailable(_))),
            "NativeOrMemory must not read NativePreferred's plaintext sidecar or claim the native grant is absent");
        isolated
            .store(
                "fixture",
                "memory-only",
                test_payload(b"synthetic-session-key"),
            )
            .await
            .unwrap();
        let memory_restarted = native_or_fallback_storage(
            missing(),
            SecureStorageBackend::LinuxLibsecret,
            Arc::new(InMemorySecureStorage::new()),
            root.path().join("locks"),
        );
        assert_eq!(
            memory_restarted.account_ownership_backend(),
            SecureStorageBackend::LinuxLibsecret
        );
        assert!(matches!(
            memory_restarted.retrieve("fixture", "memory-only").await,
            Err(SecureStorageError::BackendUnavailable(_))
        ));

        let restarted = native_or_fallback_storage(
            missing(),
            SecureStorageBackend::LinuxLibsecret,
            Arc::new(DeferredPlainTextStorage::new(plaintext.clone())),
            root.path().join("locks"),
        );
        assert!(restarted
            .retrieve("fixture", "key")
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            restarted.account_ownership_backend(),
            SecureStorageBackend::LinuxLibsecret
        );
        let recovered = native_or_fallback_storage(
            Ok(Arc::new(
                MemoryStorage::new(true).with_backend(SecureStorageBackend::LinuxLibsecret),
            )),
            SecureStorageBackend::LinuxLibsecret,
            Arc::new(DeferredPlainTextStorage::new(plaintext)),
            root.path().join("locks"),
        );
        assert!(recovered
            .retrieve("fixture", "key")
            .await
            .unwrap()
            .is_some());
        assert_eq!(recovered.backend(), SecureStorageBackend::LinuxLibsecret);
        assert_eq!(
            recovered.account_ownership_backend(),
            SecureStorageBackend::LinuxLibsecret
        );
        let fixture = secure_storage_for_policy(
            "synthetic".into(),
            root.path().to_owned(),
            root.path().join("explicit-fixture"),
            CredentialStoragePolicy::PlainTextFixture,
        )
        .await
        .unwrap();
        assert_eq!(
            fixture.account_ownership_backend(),
            SecureStorageBackend::PlainText
        );
    }

    #[test]
    fn fallback_uses_a_sidecar_directory_without_colliding_with_oauth_json() {
        let path = PathBuf::from("/tmp/.credentials.json");
        assert_eq!(
            fallback_directory(&path),
            PathBuf::from("/tmp/.credentials.json.d")
        );
    }

    #[tokio::test]
    async fn runtime_failure_uses_the_same_fallback_across_fresh_handles() {
        let locks = tempdir().expect("credential lock fixture");
        let dir = tempdir().expect("tempdir");
        let fallback_dir = dir.path().join(".credentials.json.d");
        let writer_fallback: Arc<dyn SecureStorage> = Arc::new(
            super::super::plaintext::PlainTextSecureStorage::new(fallback_dir.clone())
                .await
                .expect("writer fallback"),
        );
        let writer = RuntimeFallbackStorage::new(
            Arc::new(UnavailableStorage),
            writer_fallback,
            locks.path().join("credential-locks"),
        );
        let expected = test_payload(b"shared-provider-key");
        writer
            .store("lingxi", "provider-key-deepseek", expected.clone())
            .await
            .expect("fallback store");
        let stored_path = fallback_dir.join("lingxi/provider-key-deepseek.json");
        let stored_mode = std::fs::metadata(&stored_path)
            .expect("stored fallback metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(stored_mode, 0o600);
        std::fs::set_permissions(&stored_path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen stored fallback for overwrite check");
        writer
            .store("lingxi", "provider-key-deepseek", expected.clone())
            .await
            .expect("fallback overwrite");
        let overwritten_mode = std::fs::metadata(&stored_path)
            .expect("overwritten fallback metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(overwritten_mode, 0o600);
        assert_eq!(writer.backend(), SecureStorageBackend::PlainText);
        assert!(!writer.is_encrypted());

        let reader_fallback: Arc<dyn SecureStorage> = Arc::new(
            super::super::plaintext::PlainTextSecureStorage::new(fallback_dir)
                .await
                .expect("reader fallback"),
        );
        let reader = RuntimeFallbackStorage::new(
            Arc::new(UnavailableStorage),
            reader_fallback,
            locks.path().join("credential-locks"),
        );
        let actual = reader
            .retrieve("lingxi", "provider-key-deepseek")
            .await
            .expect("cold fallback retrieve")
            .expect("stored key");
        assert_eq!(actual.expose_secret_bytes(), expected.expose_secret_bytes());
        assert_eq!(reader.backend(), SecureStorageBackend::PlainText);
        assert!(!reader.is_encrypted());
    }

    #[tokio::test]
    async fn native_write_cannot_leave_a_stale_fallback_shadow() {
        let locks = tempdir().expect("credential lock fixture");
        let primary = Arc::new(MemoryStorage::new(true));
        let fallback = Arc::new(MemoryStorage::new(false));
        fallback
            .store(
                "lingxi",
                "provider-key-deepseek",
                test_payload(b"old-fallback-key"),
            )
            .await
            .expect("seed fallback");
        fallback.set_fail_delete(true);

        let writer = RuntimeFallbackStorage::new(
            primary.clone(),
            fallback.clone(),
            locks.path().join("credential-locks"),
        );
        let replacement = test_payload(b"new-shared-key");
        writer
            .store("lingxi", "provider-key-deepseek", replacement.clone())
            .await
            .expect("native write with fallback refresh");
        assert!(!writer.is_encrypted());

        let fresh =
            RuntimeFallbackStorage::new(primary, fallback, locks.path().join("credential-locks"));
        let actual = fresh
            .retrieve("lingxi", "provider-key-deepseek")
            .await
            .expect("fresh retrieve")
            .expect("replacement key");
        assert_eq!(
            actual.expose_secret_bytes(),
            replacement.expose_secret_bytes()
        );
    }

    #[tokio::test]
    async fn healthy_native_store_migrates_a_cold_fallback_value() {
        let locks = tempdir().expect("credential lock fixture");
        let primary = Arc::new(MemoryStorage::new(true));
        let fallback = Arc::new(MemoryStorage::new(false));
        let expected = test_payload(b"old-fallback-key");
        fallback
            .store("lingxi", "provider-key-deepseek", expected.clone())
            .await
            .expect("seed fallback");

        let storage = RuntimeFallbackStorage::new(
            primary.clone(),
            fallback.clone(),
            locks.path().join("credential-locks"),
        );
        let actual = storage
            .retrieve("lingxi", "provider-key-deepseek")
            .await
            .expect("retrieve and migrate")
            .expect("stored key");

        assert_eq!(actual.expose_secret_bytes(), expected.expose_secret_bytes());
        assert_eq!(
            primary
                .retrieve("lingxi", "provider-key-deepseek")
                .await
                .expect("native retrieve")
                .expect("migrated native key")
                .expose_secret_bytes(),
            expected.expose_secret_bytes()
        );
        assert!(
            fallback
                .retrieve("lingxi", "provider-key-deepseek")
                .await
                .expect("fallback retrieve")
                .is_none(),
            "successful migration must remove the plaintext shadow"
        );
        assert!(storage.is_encrypted());
    }

    #[tokio::test]
    async fn delete_tombstone_prevents_native_credential_resurrection() {
        let locks = tempdir().expect("credential lock fixture");
        let primary = Arc::new(MemoryStorage::new(true));
        let fallback = Arc::new(MemoryStorage::new(false));
        primary
            .store(
                "lingxi",
                "provider-key-deepseek",
                test_payload(b"old-native-key"),
            )
            .await
            .expect("seed primary");
        fallback
            .store(
                "lingxi",
                "provider-key-deepseek",
                test_payload(b"newer-fallback-key"),
            )
            .await
            .expect("seed fallback");
        primary.set_unavailable(true);

        let deleting = RuntimeFallbackStorage::new(
            primary.clone(),
            fallback.clone(),
            locks.path().join("credential-locks"),
        );
        deleting
            .delete("lingxi", "provider-key-deepseek")
            .await
            .expect("logical delete through tombstone");
        assert!(!deleting
            .contains("lingxi", "provider-key-deepseek")
            .await
            .unwrap());

        primary.set_unavailable(false);
        let recovered = RuntimeFallbackStorage::new(
            primary.clone(),
            fallback.clone(),
            locks.path().join("credential-locks"),
        );
        assert!(
            recovered
                .retrieve("lingxi", "provider-key-deepseek")
                .await
                .expect("retrieve after native recovery")
                .is_none(),
            "fallback tombstone must suppress the stale native credential"
        );
        assert!(
            recovered
                .list("lingxi")
                .await
                .expect("list after delete")
                .is_empty(),
            "deleted accounts must not leak through list"
        );
        assert!(!recovered
            .contains("lingxi", "provider-key-deepseek")
            .await
            .unwrap());

        let replacement = test_payload(b"replacement-after-delete");
        recovered
            .store("lingxi", "provider-key-deepseek", replacement.clone())
            .await
            .expect("replace tombstone");
        let fresh =
            RuntimeFallbackStorage::new(primary, fallback, locks.path().join("credential-locks"));
        let actual = fresh
            .retrieve("lingxi", "provider-key-deepseek")
            .await
            .expect("fresh retrieve after replacement")
            .expect("replacement");
        assert_eq!(
            actual.expose_secret_bytes(),
            replacement.expose_secret_bytes()
        );
        assert!(fresh.is_encrypted());
        assert!(fresh
            .contains("lingxi", "provider-key-deepseek")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn empty_fallback_does_not_turn_unknown_native_presence_into_missing() {
        let root = tempdir().unwrap();
        let primary = Arc::new(MemoryStorage::new(true));
        let storage = RuntimeFallbackStorage::new(
            primary.clone(),
            Arc::new(MemoryStorage::new(false)),
            root.path().join("locks"),
        );
        primary.set_unavailable(true);
        assert!(storage.contains("fixture", "key").await.is_err());
        assert!(storage.retrieve("fixture", "key").await.is_err());
        assert!(storage.retrieve_uncached("fixture", "key").await.is_err());
        storage
            .store("fixture", "key", test_payload(b"fallback-key"))
            .await
            .unwrap();
        assert!(storage.contains("fixture", "key").await.unwrap());
        assert!(storage.retrieve("fixture", "key").await.unwrap().is_some());
        storage.delete("fixture", "key").await.unwrap();
        assert!(!storage.contains("fixture", "key").await.unwrap());
        assert!(storage.retrieve("fixture", "key").await.unwrap().is_none());
        primary.set_unavailable(false);
        assert!(!storage.contains("fixture", "key").await.unwrap());
    }

    #[tokio::test]
    async fn healthy_native_store_does_not_create_the_fallback_directory() {
        let locks = tempdir().expect("credential lock fixture");
        let dir = tempdir().expect("tempdir");
        let fallback_dir = dir.path().join(".credentials.json.d");
        let primary = Arc::new(MemoryStorage::new(true));
        let expected = test_payload(b"native-only-key");
        primary
            .store("lingxi", "provider-key-deepseek", expected.clone())
            .await
            .expect("seed primary");
        let fallback: Arc<dyn SecureStorage> =
            Arc::new(DeferredPlainTextStorage::new(fallback_dir.clone()));
        let storage =
            RuntimeFallbackStorage::new(primary, fallback, locks.path().join("credential-locks"));

        let actual = storage
            .retrieve("lingxi", "provider-key-deepseek")
            .await
            .expect("native retrieve")
            .expect("native key");
        assert_eq!(actual.expose_secret_bytes(), expected.expose_secret_bytes());
        assert!(
            !fallback_dir.exists(),
            "native-only use must not initialize the fallback"
        );
    }

    #[tokio::test]
    async fn native_or_memory_runtime_fallback_stays_in_process_memory() {
        let locks = tempdir().expect("credential lock fixture");
        let fallback: Arc<dyn SecureStorage> = Arc::new(InMemorySecureStorage::new());
        let storage = RuntimeFallbackStorage::new(
            Arc::new(UnavailableStorage),
            fallback.clone(),
            locks.path().join("credential-locks"),
        );
        let expected = test_payload(b"packaged-bridge-session-only");

        storage
            .store("lingxi", "provider-key-openai", expected.clone())
            .await
            .expect("fallback store");
        assert_eq!(storage.backend(), SecureStorageBackend::MemorySession);
        assert!(!storage.is_encrypted());

        let actual = fallback
            .retrieve("lingxi", "provider-key-openai")
            .await
            .expect("fallback retrieve")
            .expect("stored key");
        assert_eq!(actual.expose_secret_bytes(), expected.expose_secret_bytes());
    }
}
