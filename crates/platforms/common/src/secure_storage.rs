//! Shared native credential fallback, migration, and logical deletion.

use async_trait::async_trait;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::{SecretKindDto, SecureStorageData, SecureStorageMetadata};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

const FALLBACK_TOMBSTONE_KIND: &str = "lingxi_deleted_credential_tombstone";

/// Native credential authority with serialized, policy-selected fallback and deletion markers.
#[derive(Clone)]
pub struct RuntimeFallbackStorage {
    primary: Arc<dyn SecureStorage>,
    fallback: Arc<dyn SecureStorage>,
    account_ownership_backend: SecureStorageBackend,
    fallback_active: Arc<AtomicBool>,
    warned: Arc<AtomicBool>,
    lock_directory: PathBuf,
}

impl RuntimeFallbackStorage {
    /// Retain the native owner across failures and later fallback migration.
    pub fn new(
        primary: Arc<dyn SecureStorage>,
        fallback: Arc<dyn SecureStorage>,
        lock_directory: PathBuf,
    ) -> Self {
        let account_ownership_backend = primary.account_ownership_backend();
        Self {
            primary,
            fallback,
            account_ownership_backend,
            fallback_active: Arc::new(AtomicBool::new(false)),
            warned: Arc::new(AtomicBool::new(false)),
            lock_directory,
        }
    }

    /// Record a failed initial native probe without dropping its retryable backend.
    #[must_use]
    pub fn with_fallback_active(self) -> Self {
        self.fallback_active.store(true, Ordering::Release);
        self.warned.store(true, Ordering::Release);
        self
    }

    fn lock_path(service: &str, account: &str) -> PathBuf {
        let mut hash = Sha256::new();
        for component in [service, account] {
            hash.update((component.len() as u64).to_be_bytes());
            hash.update(component.as_bytes());
        }
        PathBuf::from(format!("{:x}.lock", hash.finalize()))
    }

    async fn transaction_lock(
        &self,
        service: &str,
        account: &str,
    ) -> Result<lingxi_core::host::RootedFileLock, SecureStorageError> {
        use lingxi_core::host::rooted_fs::{self, RootedFileLockAttempt};
        let directory = self.lock_directory.clone();
        let relative = Self::lock_path(service, account);
        let mut request = tokio::task::spawn_blocking(move || {
            let parent = directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent)
                .map_err(|error| SecureStorageError::Io(error.to_string()))?;
            let parent = parent
                .canonicalize()
                .map_err(|error| SecureStorageError::Io(error.to_string()))?;
            let leaf = Path::new(directory.file_name().ok_or_else(|| {
                SecureStorageError::Io("credential lock directory is invalid".into())
            })?);
            let identity = rooted_fs::ensure_private_directory(&parent, leaf, 0o700)
                .map_err(|error| SecureStorageError::Io(error.to_string()))?;
            let directory = parent.join(leaf);
            rooted_fs::prepare_lock_exclusive_pinned(
                &directory,
                &relative,
                0o700,
                0o600,
                Some(&identity),
            )
            .map_err(|error| SecureStorageError::Io(error.to_string()))
        })
        .await
        .map_err(|error| SecureStorageError::Io(error.to_string()))??;
        loop {
            // Keep one opened lock handle across attempts. A cancelled waiter
            // drops only its own request, never the active transaction's lock.
            match tokio::task::spawn_blocking(move || request.try_acquire())
                .await
                .map_err(|error| SecureStorageError::Io(error.to_string()))?
                .map_err(|error| SecureStorageError::Io(error.to_string()))?
            {
                RootedFileLockAttempt::Acquired(lock) => return Ok(lock),
                RootedFileLockAttempt::Contended(pending) => request = pending,
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn transaction<T, F, Fut>(
        &self,
        service: &str,
        account: &str,
        operation: F,
    ) -> Result<T, SecureStorageError>
    where
        T: Send + 'static,
        F: FnOnce(Self, String, String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, SecureStorageError>> + Send + 'static,
    {
        let lock = self.transaction_lock(service, account).await?;
        let storage = self.clone();
        let service = service.to_owned();
        let account = account.to_owned();
        // Fallback reads can migrate into the native store. Once admitted,
        // every mutation owns its cross-process lock through real completion,
        // even if its caller cancels while a backend command is still running.
        tokio::spawn(async move {
            let _lock = lock;
            operation(storage, service, account).await
        })
        .await
        .map_err(|error| SecureStorageError::Io(error.to_string()))?
    }

    async fn retrieve_primary(
        &self,
        service: &str,
        account: &str,
        uncached: bool,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        if uncached {
            self.primary.retrieve_uncached(service, account).await
        } else {
            self.primary.retrieve(service, account).await
        }
    }

    async fn store_with_fallback(
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

    async fn retrieve_with_fallback(
        &self,
        service: &str,
        account: &str,
        uncached: bool,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let fallback_result = if uncached {
            self.fallback.retrieve_uncached(service, account).await
        } else {
            self.fallback.retrieve(service, account).await
        };
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
                return match self.retrieve_primary(service, account, uncached).await {
                    Ok(data) => Ok(data),
                    Err(primary_error) if runtime_fallback_allowed(&primary_error) => {
                        self.activate_fallback(&primary_error);
                        Err(fallback_error)
                    }
                    Err(primary_error) => Err(primary_error),
                };
            }
        }
        match self.retrieve_primary(service, account, uncached).await {
            Ok(data) => Ok(data),
            Err(error) if runtime_fallback_allowed(&error) => {
                self.activate_fallback(&error);
                // An empty fallback is not evidence that the inaccessible
                // native credential is missing. Preserve the unavailable
                // result so account consumers do not request a new login.
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    async fn delete_with_fallback(
        &self,
        service: &str,
        account: &str,
    ) -> Result<(), SecureStorageError> {
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

    async fn contains_with_fallback(
        &self,
        service: &str,
        account: &str,
    ) -> Result<bool, SecureStorageError> {
        // Fallback rows also encode logical deletion. Only fallback payloads
        // need inspection; a healthy native store keeps its attribute-only
        // presence query and never decrypts the secret on this path.
        match self.fallback.retrieve(service, account).await {
            Ok(Some(data)) => {
                self.fallback_active.store(true, Ordering::Release);
                Ok(!is_fallback_tombstone(&data))
            }
            Ok(None) => match self.primary.contains(service, account).await {
                Ok(present) => {
                    self.fallback_active.store(false, Ordering::Release);
                    Ok(present)
                }
                Err(error) if runtime_fallback_allowed(&error) => {
                    self.activate_fallback(&error);
                    Err(error)
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
        self.transaction(
            service,
            account,
            move |storage, service, account| async move {
                storage.store_with_fallback(&service, &account, data).await
            },
        )
        .await
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.transaction(service, account, |storage, service, account| async move {
            storage
                .retrieve_with_fallback(&service, &account, false)
                .await
        })
        .await
    }

    async fn retrieve_uncached(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.transaction(service, account, |storage, service, account| async move {
            storage
                .retrieve_with_fallback(&service, &account, true)
                .await
        })
        .await
    }

    async fn contains(&self, service: &str, account: &str) -> Result<bool, SecureStorageError> {
        self.transaction(service, account, |storage, service, account| async move {
            storage.contains_with_fallback(&service, &account).await
        })
        .await
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.transaction(service, account, |storage, service, account| async move {
            storage.delete_with_fallback(&service, &account).await
        })
        .await
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

    fn account_ownership_backend(&self) -> SecureStorageBackend {
        self.account_ownership_backend
    }
}
