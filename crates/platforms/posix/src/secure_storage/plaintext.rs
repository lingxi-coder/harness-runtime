//! Plain-text file-based [`SecureStorage`] for desktop hosts.
//!
//! Used as a fallback on Linux (no libsecret wiring yet) and as the safety
//! net on macOS when [`super::macos::MacOsKeychainStorage::new`] cannot
//! initialise. The macOS keychain backend lives in [`super::macos`].
//!
//! Stores secrets as JSON files under `<base>/<service>/<account>.json`.
//! This is NOT secure on shared systems.

use async_trait::async_trait;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::SecureStorageData;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

/// Plain-text file-based secure storage rooted at a base directory.
pub struct PlainTextSecureStorage {
    base_dir: PathBuf,
}

impl PlainTextSecureStorage {
    /// Create a new plain-text store rooted at `base_dir`.
    ///
    /// Ensures the base directory exists (creating it if necessary).
    ///
    /// # Errors
    /// Returns [`SecureStorageError::Io`] if the base directory cannot be
    /// created.
    pub async fn new(base_dir: PathBuf) -> Result<Self, SecureStorageError> {
        tokio::fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        tokio::fs::set_permissions(&base_dir, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        Ok(Self { base_dir })
    }

    fn path_for(&self, service: &str, account: &str) -> PathBuf {
        self.base_dir.join(service).join(format!("{account}.json"))
    }
}

#[async_trait]
impl SecureStorage for PlainTextSecureStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        let path = self.path_for(service, account);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| SecureStorageError::Io(e.to_string()))?;
            tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        }
        let json =
            serde_json::to_string(&data).map_err(|e| SecureStorageError::Io(e.to_string()))?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .await
            .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        file.write_all(json.as_bytes())
            .await
            .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        file.flush()
            .await
            .map_err(|e| SecureStorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let path = self.path_for(service, account);
        match tokio::fs::read_to_string(&path).await {
            Ok(json) => {
                let data: SecureStorageData = serde_json::from_str(&json)
                    .map_err(|e| SecureStorageError::Io(e.to_string()))?;
                Ok(Some(data))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SecureStorageError::Io(e.to_string())),
        }
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        let path = self.path_for(service, account);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SecureStorageError::Io(e.to_string())),
        }
    }

    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        let dir = self.base_dir.join(service);
        match tokio::fs::read_dir(&dir).await {
            Ok(mut read) => {
                let mut out = Vec::new();
                while let Some(entry) = read
                    .next_entry()
                    .await
                    .map_err(|e| SecureStorageError::Io(e.to_string()))?
                {
                    if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                        out.push(stem.to_string());
                    }
                }
                Ok(out)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(SecureStorageError::Io(e.to_string())),
        }
    }

    fn is_encrypted(&self) -> bool {
        false
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::PlainText
    }
}
