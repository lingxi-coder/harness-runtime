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
use platform_api::{
    CredentialStoragePolicy, InMemorySecureStorage, SecureStorage, SecureStorageBackend,
    SecureStorageError,
};
use protocol::{SecretKindDto, SecureStorageData, SecureStorageMetadata};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::OnceCell;

const FALLBACK_TOMBSTONE_KIND: &str = "lingxi_deleted_credential_tombstone";

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

struct RuntimeFallbackStorage {
    primary: Arc<dyn SecureStorage>,
    fallback: Arc<dyn SecureStorage>,
    fallback_active: AtomicBool,
    warned: AtomicBool,
}

impl RuntimeFallbackStorage {
    fn new(primary: Arc<dyn SecureStorage>, fallback: Arc<dyn SecureStorage>) -> Self {
        Self {
            primary,
            fallback,
            fallback_active: AtomicBool::new(false),
            warned: AtomicBool::new(false),
        }
    }

    async fn contains_with_fallback(
        &self,
        service: &str,
        account: &str,
    ) -> Result<bool, SecureStorageError> {
        match self.fallback.contains(service, account).await {
            Ok(true) => {
                self.fallback_active.store(true, Ordering::Release);
                Ok(true)
            }
            Ok(false) => match self.primary.contains(service, account).await {
                Ok(present) => {
                    self.fallback_active.store(false, Ordering::Release);
                    Ok(present)
                }
                Err(error) if runtime_fallback_allowed(&error) => {
                    self.activate_fallback(&error);
                    Ok(false)
                }
                Err(error) => Err(error),
            },
            Err(fallback_error) => match self.primary.contains(service, account).await {
                Ok(present) => Ok(present),
                Err(primary_error) if runtime_fallback_allowed(&primary_error) => {
                    self.activate_fallback(&primary_error);
                    Err(fallback_error)
                }
                Err(primary_error) => Err(primary_error),
            },
        }
    }

    fn activate_fallback(&self, error: &SecureStorageError) {
        self.fallback_active.store(true, Ordering::Release);
        if !self.warned.swap(true, Ordering::AcqRel) {
            let fallback_label = self.fallback_label();
            tracing::warn!(
                target: "lingxi::secure_storage",
                %error,
                fallback = fallback_label,
                "Warning: native credential storage is unavailable; switching fallback backend."
            );
        }
    }

    fn mark_fallback_active(&self, error: &SecureStorageError, message: &'static str) {
        self.fallback_active.store(true, Ordering::Release);
        if !self.warned.swap(true, Ordering::AcqRel) {
            let fallback_label = self.fallback_label();
            tracing::warn!(
                target: "lingxi::secure_storage",
                %error,
                fallback = fallback_label,
                "{message}"
            );
        }
    }

    fn fallback_label(&self) -> &'static str {
        match self.fallback.backend() {
            SecureStorageBackend::MemorySession => "current-session-memory",
            _ => "owner-only-plaintext",
        }
    }
}

fn runtime_fallback_allowed(error: &SecureStorageError) -> bool {
    matches!(
        error,
        SecureStorageError::BackendUnavailable(_)
            | SecureStorageError::PermissionDenied(_)
            | SecureStorageError::Io(_)
    )
}

fn fallback_tombstone() -> SecureStorageData {
    SecureStorageData::new(
        Vec::new(),
        SecureStorageMetadata {
            created_at: SystemTime::now(),
            last_accessed: None,
            kind: SecretKindDto(FALLBACK_TOMBSTONE_KIND.to_string()),
        },
    )
}

fn is_fallback_tombstone(data: &SecureStorageData) -> bool {
    data.metadata.kind.0 == FALLBACK_TOMBSTONE_KIND
}

#[async_trait]
impl SecureStorage for RuntimeFallbackStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        match self.primary.store(service, account, data.clone()).await {
            Ok(()) => match self.fallback.delete(service, account).await {
                Ok(()) => {
                    self.fallback_active.store(false, Ordering::Release);
                    Ok(())
                }
                Err(cleanup_error) => {
                    self.fallback
                            .store(service, account, data)
                            .await
                            .map_err(|refresh_error| {
                                SecureStorageError::Io(format!(
                                    "native credential stored, but fallback cleanup failed \
                                     ({cleanup_error}) and fallback refresh failed ({refresh_error})"
                                ))
                            })?;
                    self.mark_fallback_active(
                        &cleanup_error,
                        "Warning: native credential storage recovered, but the owner-only \
                             fallback could not be removed; retaining the current value in both \
                             stores.",
                    );
                    Ok(())
                }
            },
            Err(error) if runtime_fallback_allowed(&error) => {
                self.activate_fallback(&error);
                self.fallback.store(service, account, data).await
            }
            Err(error) => Err(error),
        }
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let fallback_result = self.fallback.retrieve(service, account).await;
        match fallback_result {
            Ok(Some(data)) => {
                self.fallback_active.store(true, Ordering::Release);
                if is_fallback_tombstone(&data) {
                    return Ok(None);
                }

                // A fallback file records a past native-store failure, not a
                // permanent policy choice. Retry Keychain on a later healthy
                // launch and remove the plaintext shadow only after the native
                // write succeeds. The valid fallback value remains readable
                // even if either recovery step fails.
                match self.primary.store(service, account, data.clone()).await {
                    Ok(()) => match self.fallback.delete(service, account).await {
                        Ok(()) => self.fallback_active.store(false, Ordering::Release),
                        Err(cleanup_error) => self.mark_fallback_active(
                            &cleanup_error,
                            "Warning: native credential storage recovered, but the owner-only \
                             fallback could not be removed; retaining the current value in both \
                             stores.",
                        ),
                    },
                    Err(error) if runtime_fallback_allowed(&error) => {
                        self.activate_fallback(&error);
                    }
                    Err(error) => self.mark_fallback_active(
                        &error,
                        "Warning: native credential migration failed; retaining the owner-only \
                         fallback value.",
                    ),
                }
                return Ok(Some(data));
            }
            Ok(None) => {}
            Err(fallback_error) => {
                return match self.primary.retrieve(service, account).await {
                    Ok(data) => Ok(data),
                    Err(primary_error) if runtime_fallback_allowed(&primary_error) => {
                        self.activate_fallback(&primary_error);
                        Err(fallback_error)
                    }
                    Err(primary_error) => Err(primary_error),
                };
            }
        }
        match self.primary.retrieve(service, account).await {
            Ok(data) => Ok(data),
            Err(error) if runtime_fallback_allowed(&error) => {
                self.activate_fallback(&error);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    async fn contains(&self, service: &str, account: &str) -> Result<bool, SecureStorageError> {
        self.contains_with_fallback(service, account).await
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        match self.primary.delete(service, account).await {
            Ok(()) => match self.fallback.delete(service, account).await {
                Ok(()) => {
                    self.fallback_active.store(false, Ordering::Release);
                    Ok(())
                }
                Err(cleanup_error) => {
                    self.fallback
                        .store(service, account, fallback_tombstone())
                        .await
                        .map_err(|marker_error| {
                            SecureStorageError::Io(format!(
                                "native credential deleted, but fallback cleanup failed \
                                 ({cleanup_error}) and tombstone write failed ({marker_error})"
                            ))
                        })?;
                    self.mark_fallback_active(
                        &cleanup_error,
                        "Warning: native credential was deleted, but the owner-only fallback \
                         could not be removed; recording a deletion tombstone.",
                    );
                    Ok(())
                }
            },
            Err(error) if runtime_fallback_allowed(&error) => {
                self.activate_fallback(&error);
                self.fallback
                    .store(service, account, fallback_tombstone())
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        let fallback_accounts = match self.fallback.list(service).await {
            Ok(accounts) => accounts,
            Err(fallback_error) => {
                return match self.primary.list(service).await {
                    Ok(accounts) => Ok(accounts),
                    Err(primary_error) if runtime_fallback_allowed(&primary_error) => {
                        self.activate_fallback(&primary_error);
                        Err(fallback_error)
                    }
                    Err(primary_error) => Err(primary_error),
                };
            }
        };
        let mut accounts = BTreeSet::new();
        let mut tombstones = BTreeSet::new();
        for account in fallback_accounts {
            match self.fallback.retrieve(service, &account).await? {
                Some(data) if is_fallback_tombstone(&data) => {
                    tombstones.insert(account);
                    self.fallback_active.store(true, Ordering::Release);
                }
                Some(_) => {
                    accounts.insert(account);
                    self.fallback_active.store(true, Ordering::Release);
                }
                None => {
                    accounts.insert(account);
                }
            }
        }
        match self.primary.list(service).await {
            Ok(primary_accounts) => accounts.extend(primary_accounts),
            Err(error) if runtime_fallback_allowed(&error) => self.activate_fallback(&error),
            Err(error) => return Err(error),
        }
        for account in tombstones {
            accounts.remove(&account);
        }
        Ok(accounts.into_iter().collect())
    }

    fn is_encrypted(&self) -> bool {
        !self.fallback_active.load(Ordering::Acquire) && self.primary.is_encrypted()
    }

    fn backend(&self) -> SecureStorageBackend {
        if self.fallback_active.load(Ordering::Acquire) {
            self.fallback.backend()
        } else {
            self.primary.backend()
        }
    }
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
        match super::linux::LinuxSecretStorage::new(
            user.clone(),
            config_dir.clone(),
            default_dir,
            String::new(),
        ) {
            Ok(secret) => {
                return Ok(Arc::new(RuntimeFallbackStorage::new(
                    Arc::new(secret),
                    fallback,
                )));
            }
            Err(e) => {
                tracing::warn!(
                    target: "lingxi::secure_storage",
                    error = %e,
                    policy = ?policy,
                    "Warning: native credential storage unavailable during init."
                );
            }
        }
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
    #[cfg(not(target_os = "macos"))]
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
    }

    impl MemoryStorage {
        fn new(encrypted: bool) -> Self {
            Self {
                entries: Mutex::new(std::collections::BTreeMap::new()),
                unavailable: AtomicBool::new(false),
                fail_delete: AtomicBool::new(false),
                encrypted,
            }
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
            if self.encrypted {
                SecureStorageBackend::MacOsKeychain
            } else {
                SecureStorageBackend::PlainText
            }
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
        let dir = tempdir().expect("tempdir");
        let fallback_dir = dir.path().join(".credentials.json.d");
        let writer_fallback: Arc<dyn SecureStorage> = Arc::new(
            super::super::plaintext::PlainTextSecureStorage::new(fallback_dir.clone())
                .await
                .expect("writer fallback"),
        );
        let writer = RuntimeFallbackStorage::new(Arc::new(UnavailableStorage), writer_fallback);
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
        let reader = RuntimeFallbackStorage::new(Arc::new(UnavailableStorage), reader_fallback);
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

        let writer = RuntimeFallbackStorage::new(primary.clone(), fallback.clone());
        let replacement = test_payload(b"new-shared-key");
        writer
            .store("lingxi", "provider-key-deepseek", replacement.clone())
            .await
            .expect("native write with fallback refresh");
        assert!(!writer.is_encrypted());

        let fresh = RuntimeFallbackStorage::new(primary, fallback);
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
        let primary = Arc::new(MemoryStorage::new(true));
        let fallback = Arc::new(MemoryStorage::new(false));
        let expected = test_payload(b"old-fallback-key");
        fallback
            .store("lingxi", "provider-key-deepseek", expected.clone())
            .await
            .expect("seed fallback");

        let storage = RuntimeFallbackStorage::new(primary.clone(), fallback.clone());
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

        let deleting = RuntimeFallbackStorage::new(primary.clone(), fallback.clone());
        deleting
            .delete("lingxi", "provider-key-deepseek")
            .await
            .expect("logical delete through tombstone");

        primary.set_unavailable(false);
        let recovered = RuntimeFallbackStorage::new(primary.clone(), fallback.clone());
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

        let replacement = test_payload(b"replacement-after-delete");
        recovered
            .store("lingxi", "provider-key-deepseek", replacement.clone())
            .await
            .expect("replace tombstone");
        let fresh = RuntimeFallbackStorage::new(primary, fallback);
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
    }

    #[tokio::test]
    async fn healthy_native_store_does_not_create_the_fallback_directory() {
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
        let storage = RuntimeFallbackStorage::new(primary, fallback);

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
        let fallback: Arc<dyn SecureStorage> = Arc::new(InMemorySecureStorage::new());
        let storage = RuntimeFallbackStorage::new(Arc::new(UnavailableStorage), fallback.clone());
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
