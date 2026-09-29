//! Platform secure-storage abstraction. Engine code stores OAuth tokens, API
//! keys, and other long-lived secrets via this trait; platform crates wrap the
//! OS keychain / credential vault while host-test builds use an in-memory or
//! encrypted-file fallback. See spec §6 (Secrets) and D17 (Runtime boundary).

use crate::types::SecureStorageData;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;
use tokio::sync::Mutex;

/// Persistent secure store for opaque secret payloads keyed by
/// `(service, account)`.
///
/// Implementations live in platform crates and front the OS-native credential
/// store. Engine code receives an `Arc<dyn SecureStorage>` and never touches a
/// concrete backend directly.
#[async_trait]
pub trait SecureStorage: Send + Sync {
    /// Persist `data` under the `(service, account)` key, overwriting any
    /// existing entry.
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError>;

    /// Look up the entry for `(service, account)`. Returns `Ok(None)` when no
    /// such entry exists (distinguishing a missing key from a backend error).
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError>;

    /// Check whether an entry exists without requiring callers to load its
    /// secret payload. Native backends should override this when their OS API
    /// can answer an attribute-only query without decrypting the value.
    async fn contains(&self, service: &str, account: &str) -> Result<bool, SecureStorageError> {
        self.retrieve(service, account)
            .await
            .map(|entry| entry.is_some())
    }

    /// Remove the entry for `(service, account)`. Removing a non-existent entry
    /// is not an error.
    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError>;

    /// List every `account` currently stored under `service`.
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError>;

    /// True when the underlying backend encrypts secrets at rest (e.g. an OS
    /// keychain or an encrypted file). False for the plaintext development
    /// fallback.
    fn is_encrypted(&self) -> bool;

    /// Concrete backend in use. Useful for logging, telemetry, and user-facing
    /// "where are my secrets stored?" diagnostics.
    fn backend(&self) -> SecureStorageBackend;
}

/// Identifies the concrete secure-storage backend in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecureStorageBackend {
    /// macOS Keychain Services.
    MacOsKeychain,
    /// Linux Secret Service via `libsecret` (`GNOME` Keyring, `KWallet`, ...).
    LinuxLibsecret,
    /// Windows Credential Vault (`CredRead`/`CredWrite`).
    WindowsCredVault,
    /// Process memory only. Never persisted to disk.
    MemorySession,
    /// Android `Keystore` system.
    AndroidKeystore,
    /// iOS Keychain Services.
    IosKeychain,
    /// File-backed store with at-rest encryption (development / Linux fallback).
    EncryptedFile,
    /// File-backed store without encryption. Development only.
    PlainText,
}

/// Host policy for selecting the credential fallback behind the shared
/// [`SecureStorage`] handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialStoragePolicy {
    /// Prefer the native credential store and fall back to the existing
    /// owner-only plaintext sidecar when the native backend is unavailable.
    NativePreferred,
    /// Prefer the native credential store and fall back only to process
    /// memory. Used by packaged desktop builds that must not write plaintext.
    NativeOrMemory,
    /// Skip native storage and use the explicit plaintext fixture directly.
    /// Tests and isolated boots opt into this on purpose.
    PlainTextFixture,
}

/// Failure modes for [`SecureStorage`] calls.
#[derive(Debug, Clone, Error)]
pub enum SecureStorageError {
    /// No entry exists for the requested `(service, account)` pair.
    #[error("not found: {service}/{account}")]
    NotFound {
        /// Service identifier that was queried.
        service: String,
        /// Account identifier that was queried.
        account: String,
    },
    /// Caller lacks the OS- or sandbox-level permission required for the
    /// operation (e.g. keychain unlock prompt was declined).
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// Backend is reachable in principle but currently unusable (locked
    /// keychain, missing daemon, etc.).
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    /// Catch-all for underlying I/O failures.
    #[error("io error: {0}")]
    Io(String),
}

/// Non-persistent in-process [`SecureStorage`] fallback.
///
/// This is intentionally separate from the plaintext file fallback: packaged
/// desktop builds may keep credentials for the current process lifetime only,
/// but must never write them to disk when the OS-native backend is unavailable.
#[derive(Default)]
pub struct InMemorySecureStorage {
    entries: Mutex<BTreeMap<(String, String), SecureStorageData>>,
}

impl InMemorySecureStorage {
    /// Construct the non-persistent current-session fallback store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SecureStorage for InMemorySecureStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
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
        Ok(self
            .entries
            .lock()
            .await
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.entries
            .lock()
            .await
            .remove(&(service.to_string(), account.to_string()));
        Ok(())
    }

    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        Ok(self
            .entries
            .lock()
            .await
            .iter()
            .filter_map(|((stored_service, account), _)| {
                (stored_service == service).then_some(account.clone())
            })
            .collect())
    }

    fn is_encrypted(&self) -> bool {
        false
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::MemorySession
    }
}
