//! Windows secure storage backends.
//!
//! Production prefers the native Windows Credential Manager backend and falls
//! back according to [`lingxi_core::host::CredentialStoragePolicy`]. Tests can still
//! opt into the existing plain-text fixture directly.

use async_trait::async_trait;
use lingxi_core::host::{
    CredentialStoragePolicy, InMemorySecureStorage, SecureStorage, SecureStorageBackend,
    SecureStorageError,
};
use lingxi_core::types::SecureStorageData;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

const CREDENTIAL_BLOB_LIMIT: usize = 2560;
const TARGET_NAMESPACE: &str = "LingXi/secure-storage/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialManifest {
    generation: u64,
    chunk_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestTarget {
    service: String,
    account: String,
}

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
        Ok(Self { base_dir })
    }

    fn path_for(&self, service: &str, account: &str) -> PathBuf {
        self.base_dir.join(service).join(format!("{account}.json"))
    }
}

/// Native Windows Credential Manager backend.
pub struct WindowsCredentialVaultStorage {
    user: String,
    config_hash: String,
    backend: Arc<dyn CredentialBackendApi>,
}

impl WindowsCredentialVaultStorage {
    /// Construct the native Credential Manager-backed store for one config root.
    #[must_use]
    pub fn new(user: String, config_dir: PathBuf) -> Self {
        Self {
            user,
            config_hash: config_hash(&config_dir),
            backend: Arc::new(RealCredentialBackend),
        }
    }

    #[cfg(test)]
    fn with_backend(
        user: String,
        config_dir: PathBuf,
        backend: Arc<dyn CredentialBackendApi>,
    ) -> Self {
        Self {
            user,
            config_hash: config_hash(&config_dir),
            backend,
        }
    }

    fn target_prefix(&self, service: &str, account: &str) -> String {
        format!(
            "{TARGET_NAMESPACE}/{}/{}/{}",
            self.config_hash,
            encode_component(service),
            encode_component(account)
        )
    }

    fn manifest_target(&self, service: &str, account: &str) -> String {
        format!("{}/manifest", self.target_prefix(service, account))
    }

    fn chunk_target(&self, service: &str, account: &str, generation: u64, index: u32) -> String {
        format!(
            "{}/chunks/{generation}/{index}",
            self.target_prefix(service, account)
        )
    }

    fn read_manifest(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<CredentialManifest>, SecureStorageError> {
        let Some(raw) = self.backend.read(&self.manifest_target(service, account))? else {
            return Ok(None);
        };
        serde_json::from_slice(&raw).map(Some).map_err(|error| {
            SecureStorageError::Io(format!("decode Windows credential manifest: {error}"))
        })
    }

    fn delete_generation(
        &self,
        service: &str,
        account: &str,
        generation: u64,
        chunk_count: u32,
    ) -> Result<(), SecureStorageError> {
        for index in 0..chunk_count {
            self.backend
                .delete(&self.chunk_target(service, account, generation, index))?;
        }
        Ok(())
    }

    fn delete_all_targets(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        let prefix = self.target_prefix(service, account);
        for target in self.backend.enumerate_targets()? {
            if target.starts_with(&prefix) {
                self.backend.delete(&target)?;
            }
        }
        Ok(())
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
        }
        let json =
            serde_json::to_string(&data).map_err(|e| SecureStorageError::Io(e.to_string()))?;
        tokio::fs::write(&path, json)
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

#[async_trait]
impl SecureStorage for WindowsCredentialVaultStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        let serialized =
            serde_json::to_vec(&data).map_err(|error| SecureStorageError::Io(error.to_string()))?;
        let old_manifest = self.read_manifest(service, account)?;
        let generation = old_manifest
            .as_ref()
            .map_or(1, |manifest| manifest.generation.saturating_add(1));
        let chunks = chunk_bytes(&serialized);
        for (index, chunk) in chunks.iter().enumerate() {
            self.backend.write(
                &self.chunk_target(service, account, generation, index as u32),
                chunk,
                &self.user,
            )?;
        }
        let manifest = CredentialManifest {
            generation,
            chunk_count: chunks.len() as u32,
        };
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|error| SecureStorageError::Io(error.to_string()))?;
        if let Err(error) = self.backend.write(
            &self.manifest_target(service, account),
            &manifest_bytes,
            &self.user,
        ) {
            let _ = self.delete_generation(service, account, generation, chunks.len() as u32);
            return Err(error);
        }
        if let Some(old) = old_manifest {
            self.delete_generation(service, account, old.generation, old.chunk_count)?;
        }
        Ok(())
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let Some(manifest) = self.read_manifest(service, account)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        for index in 0..manifest.chunk_count {
            let Some(chunk) = self.backend.read(&self.chunk_target(
                service,
                account,
                manifest.generation,
                index,
            ))?
            else {
                return Err(SecureStorageError::Io(format!(
                    "missing Windows credential chunk {index} for {service}/{account}"
                )));
            };
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| SecureStorageError::Io(format!("decode Windows credential: {error}")))
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        let manifest = self.read_manifest(service, account)?;
        self.backend
            .delete(&self.manifest_target(service, account))?;
        if let Some(manifest) = manifest {
            self.delete_generation(service, account, manifest.generation, manifest.chunk_count)?;
        } else {
            self.delete_all_targets(service, account)?;
        }
        Ok(())
    }

    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        let service_prefix = format!(
            "{TARGET_NAMESPACE}/{}/{}",
            self.config_hash,
            encode_component(service)
        );
        let mut accounts = BTreeSet::new();
        for target in self.backend.enumerate_targets()? {
            let Some(parsed) = parse_manifest_target(&target) else {
                continue;
            };
            if !target.starts_with(&service_prefix) {
                continue;
            }
            if parsed.service == service {
                accounts.insert(parsed.account.to_string());
            }
        }
        Ok(accounts.into_iter().collect())
    }

    fn is_encrypted(&self) -> bool {
        true
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::WindowsCredVault
    }
}

fn fallback_directory(plaintext_path: &std::path::Path) -> PathBuf {
    let mut name = plaintext_path
        .file_name()
        .map_or_else(|| OsString::from("credentials"), OsString::from);
    name.push(".d");
    plaintext_path.with_file_name(name)
}

/// Build the explicit plaintext fixture, skipping Windows Credential Manager.
pub async fn plaintext_secure_storage(
    plaintext_path: PathBuf,
) -> Result<Arc<dyn SecureStorage>, SecureStorageError> {
    Ok(Arc::new(PlainTextSecureStorage::new(plaintext_path).await?))
}

/// Build the shared Windows credential store using the requested fallback
/// policy.
pub async fn secure_storage_for_policy(
    user: String,
    config_dir: PathBuf,
    plaintext_path: PathBuf,
    policy: CredentialStoragePolicy,
) -> Result<Arc<dyn SecureStorage>, SecureStorageError> {
    match policy {
        CredentialStoragePolicy::PlainTextFixture => plaintext_secure_storage(plaintext_path).await,
        CredentialStoragePolicy::NativePreferred => {
            let fallback =
                Arc::new(PlainTextSecureStorage::new(fallback_directory(&plaintext_path)).await?);
            build_native_or_fallback(user, config_dir, fallback).await
        }
        CredentialStoragePolicy::NativeOrMemory => {
            let fallback = Arc::new(InMemorySecureStorage::new());
            build_native_or_fallback(user, config_dir, fallback).await
        }
    }
}

/// Build the shared Windows credential store with the production
/// `NativePreferred` policy.
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

async fn build_native_or_fallback(
    user: String,
    config_dir: PathBuf,
    fallback: Arc<dyn SecureStorage>,
) -> Result<Arc<dyn SecureStorage>, SecureStorageError> {
    match build_native_storage(user, config_dir) {
        Ok(storage) => Ok(Arc::new(storage)),
        Err(error) => {
            tracing::warn!(
                target: "lingxi::secure_storage",
                %error,
                fallback = ?fallback.backend(),
                "Windows native credential storage unavailable during init."
            );
            Ok(fallback)
        }
    }
}

fn build_native_storage(
    user: String,
    config_dir: PathBuf,
) -> Result<WindowsCredentialVaultStorage, SecureStorageError> {
    let storage = WindowsCredentialVaultStorage::new(user, config_dir);
    let _ = storage.backend.enumerate_targets()?;
    Ok(storage)
}

fn encode_component(value: &str) -> String {
    hex::encode(value.as_bytes())
}

fn decode_component(value: &str) -> Option<String> {
    let bytes = hex::decode(value).ok()?;
    String::from_utf8(bytes).ok()
}

fn config_hash(config_dir: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)[..8].to_string()
}

fn chunk_bytes(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return vec![&[]];
    }
    bytes.chunks(CREDENTIAL_BLOB_LIMIT).collect()
}

fn parse_manifest_target(target: &str) -> Option<ManifestTarget> {
    let mut parts = target.split('/');
    if parts.next()? != "LingXi" {
        return None;
    }
    if parts.next()? != "secure-storage" {
        return None;
    }
    if parts.next()? != "v1" {
        return None;
    }
    let _config_hash = parts.next()?;
    let service = decode_component(parts.next()?)?;
    let account = decode_component(parts.next()?)?;
    if parts.next()? != "manifest" {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(ManifestTarget { service, account })
}

trait CredentialBackendApi: Send + Sync {
    fn write(&self, target: &str, blob: &[u8], username: &str) -> Result<(), SecureStorageError>;
    fn read(&self, target: &str) -> Result<Option<Vec<u8>>, SecureStorageError>;
    fn delete(&self, target: &str) -> Result<(), SecureStorageError>;
    fn enumerate_targets(&self) -> Result<Vec<String>, SecureStorageError>;
}

struct RealCredentialBackend;

impl CredentialBackendApi for RealCredentialBackend {
    fn write(&self, target: &str, blob: &[u8], username: &str) -> Result<(), SecureStorageError> {
        wincred::write(target, blob, username)
    }

    fn read(&self, target: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        wincred::read(target)
    }

    fn delete(&self, target: &str) -> Result<(), SecureStorageError> {
        wincred::delete(target)
    }

    fn enumerate_targets(&self) -> Result<Vec<String>, SecureStorageError> {
        wincred::enumerate_targets()
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod wincred {
    use super::CREDENTIAL_BLOB_LIMIT;
    use lingxi_core::host::SecureStorageError;
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW, CREDENTIALW,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    pub(super) fn write(
        target: &str,
        blob: &[u8],
        username: &str,
    ) -> Result<(), SecureStorageError> {
        if blob.len() > CREDENTIAL_BLOB_LIMIT {
            return Err(SecureStorageError::Io(format!(
                "Windows credential blob exceeds {CREDENTIAL_BLOB_LIMIT} bytes"
            )));
        }
        let mut target_wide = wide(target);
        let mut username_wide = wide(username);
        // SAFETY: `CREDENTIALW` is a plain C struct. Zero-initializing it is
        // valid, and every pointer field below is filled with a stable, NUL-
        // terminated allocation that lives through the API call.
        let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target_wide.as_mut_ptr();
        credential.UserName = username_wide.as_mut_ptr();
        credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
        credential.CredentialBlobSize = blob.len() as u32;
        credential.CredentialBlob = if blob.is_empty() {
            null_mut()
        } else {
            blob.as_ptr() as *mut u8
        };
        // SAFETY: pointers in `credential` remain valid for the duration of the
        // call, and `CredWriteW` copies the target/blob into the user vault.
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(last_error(format!("CredWriteW({target})")));
        }
        Ok(())
    }

    pub(super) fn read(target: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        let target_wide = wide(target);
        let mut credential_ptr: *mut CREDENTIALW = null_mut();
        // SAFETY: `target_wide` is NUL-terminated and `credential_ptr` is a
        // valid out-pointer for the duration of the call.
        if unsafe {
            CredReadW(
                target_wide.as_ptr(),
                CRED_TYPE_GENERIC,
                0,
                &mut credential_ptr,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                return Ok(None);
            }
            return Err(last_error(format!("CredReadW({target})")));
        }
        let credential = ScopedCredential(credential_ptr);
        // SAFETY: `credential.0` is owned by `ScopedCredential`, guaranteed non-null
        // on success, and `CredentialBlobSize` bytes are valid until `CredFree`.
        let bytes = unsafe {
            let credential_ref = &*credential.0;
            let size = credential_ref.CredentialBlobSize as usize;
            if size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(credential_ref.CredentialBlob as *const u8, size)
                    .to_vec()
            }
        };
        Ok(Some(bytes))
    }

    pub(super) fn delete(target: &str) -> Result<(), SecureStorageError> {
        let target_wide = wide(target);
        // SAFETY: `target_wide` is NUL-terminated and valid for the duration of
        // the call. Missing credentials are treated as a no-op by the caller.
        if unsafe { CredDeleteW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                return Ok(());
            }
            return Err(last_error(format!("CredDeleteW({target})")));
        }
        Ok(())
    }

    pub(super) fn enumerate_targets() -> Result<Vec<String>, SecureStorageError> {
        let mut count = 0u32;
        let mut credentials_ptr: *mut *mut CREDENTIALW = null_mut();
        // SAFETY: a null filter enumerates the current logon session's generic
        // credentials; the out-pointers are valid for the duration of the call.
        if unsafe { CredEnumerateW(null(), 0, &mut count, &mut credentials_ptr) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                return Ok(Vec::new());
            }
            return Err(last_error("CredEnumerateW".to_string()));
        }
        let credentials = ScopedCredentials(credentials_ptr);
        let mut targets = Vec::with_capacity(count as usize);
        // SAFETY: the API returned `count` entries rooted at `credentials.0`,
        // valid until `CredFree` in `ScopedCredentials::drop`.
        let slice = unsafe { std::slice::from_raw_parts(credentials.0, count as usize) };
        for credential_ptr in slice {
            // SAFETY: each pointer in the returned slice is valid for the same
            // lifetime as the root credentials allocation.
            let target = unsafe { wide_ptr_to_string((**credential_ptr).TargetName) };
            targets.push(target);
        }
        Ok(targets)
    }

    struct ScopedCredential(*mut CREDENTIALW);

    impl Drop for ScopedCredential {
        fn drop(&mut self) {
            // SAFETY: successful `CredReadW` transfers ownership of the pointer
            // to the caller, which must release it with `CredFree`.
            unsafe { CredFree(self.0 as *mut c_void) };
        }
    }

    struct ScopedCredentials(*mut *mut CREDENTIALW);

    impl Drop for ScopedCredentials {
        fn drop(&mut self) {
            // SAFETY: successful `CredEnumerateW` transfers ownership of the
            // root pointer to the caller, which must release it with `CredFree`.
            unsafe { CredFree(self.0 as *mut c_void) };
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    unsafe fn wide_ptr_to_string(mut ptr: *const u16) -> String {
        let start = ptr;
        let mut len = 0usize;
        while !ptr.is_null() && *ptr != 0 {
            len += 1;
            ptr = ptr.add(1);
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(start, len))
    }

    fn last_error(context: String) -> SecureStorageError {
        let error = io::Error::last_os_error();
        SecureStorageError::BackendUnavailable(format!("{context}: {error}"))
    }
}

#[cfg(not(target_os = "windows"))]
mod wincred {
    use lingxi_core::host::SecureStorageError;

    pub(super) fn write(
        _target: &str,
        _blob: &[u8],
        _username: &str,
    ) -> Result<(), SecureStorageError> {
        Err(SecureStorageError::BackendUnavailable(
            "Windows Credential Manager is unavailable on non-Windows hosts".to_string(),
        ))
    }

    pub(super) fn read(_target: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
        Err(SecureStorageError::BackendUnavailable(
            "Windows Credential Manager is unavailable on non-Windows hosts".to_string(),
        ))
    }

    pub(super) fn delete(_target: &str) -> Result<(), SecureStorageError> {
        Err(SecureStorageError::BackendUnavailable(
            "Windows Credential Manager is unavailable on non-Windows hosts".to_string(),
        ))
    }

    pub(super) fn enumerate_targets() -> Result<Vec<String>, SecureStorageError> {
        Err(SecureStorageError::BackendUnavailable(
            "Windows Credential Manager is unavailable on non-Windows hosts".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{SecretKindDto, SecureStorageMetadata};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[derive(Default)]
    struct MockCredentialBackend {
        entries: Mutex<BTreeMap<String, Vec<u8>>>,
        fail_writes: Mutex<BTreeSet<String>>,
        deleted_targets: Mutex<Vec<String>>,
    }

    impl MockCredentialBackend {
        fn fail_on_write(&self, target: String) {
            self.fail_writes.lock().unwrap().insert(target);
        }

        fn has_target(&self, target: &str) -> bool {
            self.entries.lock().unwrap().contains_key(target)
        }

        fn deleted_targets(&self) -> Vec<String> {
            self.deleted_targets.lock().unwrap().clone()
        }
    }

    impl CredentialBackendApi for MockCredentialBackend {
        fn write(
            &self,
            target: &str,
            blob: &[u8],
            _username: &str,
        ) -> Result<(), SecureStorageError> {
            if self.fail_writes.lock().unwrap().contains(target) {
                return Err(SecureStorageError::BackendUnavailable(format!(
                    "mock write failure for {target}"
                )));
            }
            self.entries
                .lock()
                .unwrap()
                .insert(target.to_string(), blob.to_vec());
            Ok(())
        }

        fn read(&self, target: &str) -> Result<Option<Vec<u8>>, SecureStorageError> {
            Ok(self.entries.lock().unwrap().get(target).cloned())
        }

        fn delete(&self, target: &str) -> Result<(), SecureStorageError> {
            self.deleted_targets
                .lock()
                .unwrap()
                .push(target.to_string());
            self.entries.lock().unwrap().remove(target);
            Ok(())
        }

        fn enumerate_targets(&self) -> Result<Vec<String>, SecureStorageError> {
            Ok(self.entries.lock().unwrap().keys().cloned().collect())
        }
    }

    fn payload(len: usize) -> SecureStorageData {
        SecureStorageData::new(
            vec![b'x'; len],
            SecureStorageMetadata {
                created_at: SystemTime::UNIX_EPOCH,
                last_accessed: None,
                kind: SecretKindDto("test".to_string()),
            },
        )
    }

    #[test]
    fn fallback_uses_sidecar_directory() {
        let path = PathBuf::from(r"C:\tmp\.credentials.json");
        assert_eq!(
            fallback_directory(&path),
            PathBuf::from(r"C:\tmp\.credentials.json.d")
        );
    }

    #[test]
    fn chunking_respects_windows_blob_limit() {
        let bytes = vec![1u8; CREDENTIAL_BLOB_LIMIT * 2 + 17];
        let chunks = chunk_bytes(&bytes);
        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= CREDENTIAL_BLOB_LIMIT));
        assert_eq!(chunks[0].len(), CREDENTIAL_BLOB_LIMIT);
        assert_eq!(chunks[1].len(), CREDENTIAL_BLOB_LIMIT);
        assert_eq!(chunks[2].len(), 17);
    }

    #[test]
    fn manifest_target_round_trips_unicode_components() {
        let storage = WindowsCredentialVaultStorage::new(
            "tester".to_string(),
            PathBuf::from(r"C:\Users\tester\.lingxi"),
        );
        let target = storage.manifest_target("服务", "用户/密钥");
        let parsed = parse_manifest_target(&target).expect("manifest target");
        assert_eq!(parsed.service, "服务");
        assert_eq!(parsed.account, "用户/密钥");
    }

    #[tokio::test]
    async fn mock_backend_roundtrip_supports_unicode_and_chunking() {
        let backend = Arc::new(MockCredentialBackend::default());
        let storage = WindowsCredentialVaultStorage::with_backend(
            "tester".to_string(),
            PathBuf::from(r"C:\Users\tester\.lingxi"),
            backend,
        );
        let service = "lingxi-服务";
        let account = "用户/密钥";
        let expected = payload(CREDENTIAL_BLOB_LIMIT * 2 + 97);

        storage
            .store(service, account, expected.clone())
            .await
            .expect("store");
        let listed = storage.list(service).await.expect("list");
        assert_eq!(listed, vec![account.to_string()]);
        let actual = storage
            .retrieve(service, account)
            .await
            .expect("retrieve")
            .expect("stored");
        assert_eq!(actual.expose_secret_bytes(), expected.expose_secret_bytes());
    }

    #[tokio::test]
    async fn manifest_write_failure_cleans_up_partial_chunks() {
        let backend = Arc::new(MockCredentialBackend::default());
        let storage = WindowsCredentialVaultStorage::with_backend(
            "tester".to_string(),
            PathBuf::from(r"C:\Users\tester\.lingxi"),
            backend.clone(),
        );
        let service = "lingxi";
        let account = "provider-key-openai";
        let manifest_target = storage.manifest_target(service, account);
        backend.fail_on_write(manifest_target.clone());

        let error = storage
            .store(service, account, payload(CREDENTIAL_BLOB_LIMIT + 10))
            .await
            .expect_err("manifest write should fail");
        assert!(matches!(error, SecureStorageError::BackendUnavailable(_)));

        let chunk0 = storage.chunk_target(service, account, 1, 0);
        let chunk1 = storage.chunk_target(service, account, 1, 1);
        assert!(!backend.has_target(&chunk0));
        assert!(!backend.has_target(&chunk1));
        assert!(!backend.has_target(&manifest_target));
        let deleted = backend.deleted_targets();
        assert!(deleted.contains(&chunk0));
        assert!(deleted.contains(&chunk1));
    }

    #[tokio::test]
    async fn native_or_memory_policy_never_uses_plaintext_on_non_windows_hosts() {
        #[cfg(target_os = "windows")]
        return;

        let dir = tempdir().expect("tempdir");
        let storage = secure_storage_for_policy(
            "tester".to_string(),
            dir.path().to_path_buf(),
            dir.path().join(".credentials.json"),
            CredentialStoragePolicy::NativeOrMemory,
        )
        .await
        .expect("storage");
        assert_eq!(storage.backend(), SecureStorageBackend::MemorySession);
        storage
            .store("lingxi", "provider-key-openai", payload(42))
            .await
            .expect("store");
        assert!(storage
            .retrieve("lingxi", "provider-key-openai")
            .await
            .expect("retrieve")
            .is_some());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_credential_vault_roundtrip_supports_unicode_and_chunking() {
        let dir = tempdir().expect("tempdir");
        let storage =
            WindowsCredentialVaultStorage::new("tester".to_string(), dir.path().to_path_buf());
        let service = "lingxi-服务";
        let account = format!("用户/密钥-{}", std::process::id());
        let expected = payload(CREDENTIAL_BLOB_LIMIT * 2 + 97);

        storage
            .store(service, &account, expected.clone())
            .await
            .expect("store");
        let listed = storage.list(service).await.expect("list");
        assert!(listed.contains(&account));
        let actual = storage
            .retrieve(service, &account)
            .await
            .expect("retrieve")
            .expect("stored");
        assert_eq!(actual.expose_secret_bytes(), expected.expose_secret_bytes());
        storage.delete(service, &account).await.expect("delete");
        assert!(storage
            .retrieve(service, &account)
            .await
            .expect("post delete")
            .is_none());
    }
}
