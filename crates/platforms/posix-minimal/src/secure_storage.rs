//! Stub [`SecureStorage`] — M1.22 reports `PlainText` backend with every
//! operation returning `BackendUnavailable`. A real plain-text-on-disk
//! implementation (and the OS keychain variants) ships in Plan 17.

use async_trait::async_trait;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::SecureStorageData;

/// Stub secure storage — declares the plaintext backend without persisting.
#[derive(Default)]
pub struct PlainTextSecureStorage;

impl PlainTextSecureStorage {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SecureStorage for PlainTextSecureStorage {
    async fn store(
        &self,
        _service: &str,
        _account: &str,
        _data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        Err(SecureStorageError::BackendUnavailable(
            "posix-minimal: storage stub (Plan 17 wires the on-disk store)".into(),
        ))
    }

    async fn retrieve(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        Ok(None)
    }

    async fn delete(&self, _service: &str, _account: &str) -> Result<(), SecureStorageError> {
        Ok(())
    }

    async fn list(&self, _service: &str) -> Result<Vec<String>, SecureStorageError> {
        Ok(vec![])
    }

    fn is_encrypted(&self) -> bool {
        false
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::PlainText
    }
}
