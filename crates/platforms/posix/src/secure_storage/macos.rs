//! macOS secure-storage backend routed through the signed LingXi credential
//! client, which forwards bounded requests to the app-bundled XPC broker.

use crate::secure_storage::helpers::KEYCHAIN_CACHE_TTL;
use async_trait::async_trait;
use base64::Engine as _;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::SecureStorageData;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};

type CacheKey = (String, String);
const BROKER_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const BROKER_RESOURCE_DIRNAME: &str = "credential-broker";
const BROKER_CLIENT_EXECUTABLE: &str = "lingxi-credential-client";
const BROKER_PROTOCOL_VERSION: u32 = 1;
const MAX_BROKER_REQUEST_BYTES: usize = 128 * 1024;
const MAX_BROKER_RESPONSE_BYTES: usize = 128 * 1024;
struct BrokerAddress {
    service: String,
    account: String,
}

#[derive(Clone)]
struct CachedEntry {
    data: SecureStorageData,
    fetched_at: Instant,
    generation: u64,
}

#[derive(Debug, Serialize)]
struct BrokerRequest<'a> {
    op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrokerResponse {
    ok: bool,
    #[serde(default)]
    present: Option<bool>,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    accounts: Option<Vec<String>>,
    #[serde(default)]
    protocol_version: Option<u32>,
    #[allow(dead_code)]
    #[serde(default)]
    build_version: Option<String>,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Clone)]
pub struct MacOsKeychainStorage {
    #[allow(dead_code)]
    user: String,
    config_dir: PathBuf,
    default_config_dir: PathBuf,
    oauth_suffix: String,
    broker_client: PathBuf,
    broker_timeout: std::time::Duration,
    cache: Arc<RwLock<HashMap<CacheKey, CachedEntry>>>,
    generation: Arc<AtomicU64>,
    inflight: Arc<Mutex<HashMap<CacheKey, Weak<KeyOperation>>>>,
}

#[derive(Default)]
struct KeyOperation {
    lock: Arc<Mutex<()>>,
    attempt: AtomicU64,
    result: std::sync::Mutex<Option<(u64, Result<Option<SecureStorageData>, SecureStorageError>)>>,
}

impl MacOsKeychainStorage {
    async fn operation_gate(&self, key: &CacheKey) -> Arc<KeyOperation> {
        let mut gates = self.inflight.lock().await;
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(KeyOperation::default());
        gates.insert(key.clone(), Arc::downgrade(&gate));
        gate
    }

    async fn retrieve_with_cache(
        &self,
        service: &str,
        account: &str,
        use_cache: bool,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let key = (service.to_string(), account.to_string());
        let gate = self.operation_gate(&key).await;
        let observed_attempt = gate.attempt.load(Ordering::Acquire);
        let _operation = gate.lock.lock().await;
        let generation = self.generation.load(Ordering::Acquire);
        if use_cache {
            if let Some(entry) = self.cache.read().await.get(&key).cloned() {
                if entry.generation == generation && entry.fetched_at.elapsed() < KEYCHAIN_CACHE_TTL
                {
                    return Ok(Some(entry.data));
                }
            }
            if gate.attempt.load(Ordering::Acquire) != observed_attempt {
                if let Some((result_generation, result)) = gate
                    .result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    if *result_generation == generation {
                        return result.clone();
                    }
                }
            }
        }
        // An authoritative miss/error must not leave the previous positive value hot.
        self.cache.write().await.remove(&key);
        let result = async {
            let address = self.broker_address(service, account);
            let request = BrokerRequest {
                op: "retrieve",
                service: Some(&address.service),
                account: Some(&address.account),
                payload: None,
            };
            let response = self.invoke_broker(&request).await?;
            require_success("retrieve", &response)?;
            let data = response
                .payload
                .as_deref()
                .map(decode_storage_payload)
                .transpose()?;
            if let Some(data) = data.clone() {
                self.cache.write().await.insert(
                    key,
                    CachedEntry {
                        data,
                        fetched_at: Instant::now(),
                        generation,
                    },
                );
            }
            Ok(data)
        }
        .await;
        *gate
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((generation, result.clone()));
        gate.attempt.fetch_add(1, Ordering::AcqRel);
        result
    }

    pub fn new(
        user: String,
        config_dir: PathBuf,
        default_config_dir: PathBuf,
        oauth_suffix: String,
    ) -> Result<Self, SecureStorageError> {
        Ok(Self {
            user,
            config_dir,
            default_config_dir,
            oauth_suffix,
            broker_client: resolve_broker_client()?,
            broker_timeout: BROKER_COMMAND_TIMEOUT,
            cache: Arc::new(RwLock::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn new_default_oauth_suffix(
        user: String,
        config_dir: PathBuf,
        default_config_dir: PathBuf,
    ) -> Result<Self, SecureStorageError> {
        Self::new(user, config_dir, default_config_dir, String::new())
    }

    pub(crate) fn keychain_service_name(&self, service_suffix: &str) -> String {
        let dir_hash = super::helpers::compute_dir_hash(
            self.config_dir.as_path(),
            self.default_config_dir.as_path(),
        );
        let suffix = if service_suffix.is_empty() {
            "default".to_string()
        } else {
            service_suffix
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                        character
                    } else {
                        '-'
                    }
                })
                .collect()
        };
        format!(
            "com.lingxi.secure-storage.v1.{}.{}{}{}",
            broker_channel(),
            self.oauth_suffix,
            suffix,
            if dir_hash.is_empty() {
                String::new()
            } else {
                format!("-{dir_hash}")
            }
        )
    }

    fn broker_address(&self, service: &str, account: &str) -> BrokerAddress {
        BrokerAddress {
            service: self.keychain_service_name(service),
            account: account.to_string(),
        }
    }

    pub(crate) fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Use a synthetic broker without resolving or accessing the native broker.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn with_broker_for_test(
        config_dir: PathBuf,
        default_config_dir: PathBuf,
        broker_client: PathBuf,
    ) -> Self {
        Self {
            user: "tester".to_string(),
            config_dir,
            default_config_dir,
            oauth_suffix: String::new(),
            broker_client,
            broker_timeout: BROKER_COMMAND_TIMEOUT,
            cache: Arc::new(RwLock::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn invoke_broker(
        &self,
        request: &BrokerRequest<'_>,
    ) -> Result<BrokerResponse, SecureStorageError> {
        let payload = serde_json::to_vec(request).map_err(|error| {
            SecureStorageError::Io(format!("serialize credential broker request: {error}"))
        })?;
        if payload.len() > MAX_BROKER_REQUEST_BYTES {
            return Err(SecureStorageError::Io(
                "credential broker request exceeded the size limit".to_string(),
            ));
        }
        let mut command = Command::new(&self.broker_client);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            SecureStorageError::BackendUnavailable(format!(
                "failed to launch the macOS credential client: {error}"
            ))
        })?;
        let output = tokio::time::timeout(self.broker_timeout, async move {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&payload).await.map_err(|error| {
                    SecureStorageError::Io(format!("credential client stdin write: {error}"))
                })?;
                stdin.shutdown().await.map_err(|error| {
                    SecureStorageError::Io(format!("credential client stdin shutdown: {error}"))
                })?;
            }
            child
                .wait_with_output()
                .await
                .map_err(|error| SecureStorageError::Io(format!("credential client wait: {error}")))
        })
        .await
        .map_err(|_| {
            SecureStorageError::BackendUnavailable("macOS credential client timed out".into())
        })??;
        if output.stdout.len() > MAX_BROKER_RESPONSE_BYTES {
            return Err(SecureStorageError::BackendUnavailable(
                "macOS credential client response exceeded the size limit".to_string(),
            ));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !output.status.success() {
            return Err(SecureStorageError::BackendUnavailable(
                if stderr.is_empty() {
                    format!(
                        "macOS credential client exited {}",
                        output.status.code().unwrap_or(-1)
                    )
                } else {
                    format!(
                        "macOS credential client exited {}: {stderr}",
                        output.status.code().unwrap_or(-1)
                    )
                },
            ));
        }
        serde_json::from_slice::<BrokerResponse>(&output.stdout).map_err(|error| {
            let suffix = if stderr.is_empty() {
                String::new()
            } else {
                format!(" ({stderr})")
            };
            SecureStorageError::BackendUnavailable(format!(
                "invalid macOS credential client response: {error}{suffix}"
            ))
        })
    }
}

#[async_trait]
impl SecureStorage for MacOsKeychainStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        let key = (service.to_owned(), account.to_owned());
        let gate = self.operation_gate(&key).await;
        let guard = Arc::clone(&gate.lock).lock_owned().await;
        let storage = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            *gate
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            gate.attempt.fetch_add(1, Ordering::AcqRel);
            storage.cache.write().await.remove(&key);
            storage.bump_generation();
            let address = storage.broker_address(&key.0, &key.1);
            let request = BrokerRequest {
                op: "store",
                service: Some(&address.service),
                account: Some(&address.account),
                payload: Some(encode_storage_payload(&data)?),
            };
            let response = storage.invoke_broker(&request).await?;
            require_success("store", &response)?;
            let generation = storage.generation.load(Ordering::Acquire);
            storage.cache.write().await.insert(
                key,
                CachedEntry {
                    data,
                    fetched_at: Instant::now(),
                    generation,
                },
            );
            Ok(())
        })
        .await
        .map_err(|error| SecureStorageError::BackendUnavailable(error.to_string()))?
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.retrieve_with_cache(service, account, true).await
    }

    async fn retrieve_uncached(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.retrieve_with_cache(service, account, false).await
    }

    async fn contains(&self, service: &str, account: &str) -> Result<bool, SecureStorageError> {
        let address = self.broker_address(service, account);
        let request = BrokerRequest {
            op: "contains",
            service: Some(&address.service),
            account: Some(&address.account),
            payload: None,
        };
        let response = self.invoke_broker(&request).await?;
        require_success("contains", &response)?;
        Ok(response.present.unwrap_or(false))
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        let key = (service.to_owned(), account.to_owned());
        let gate = self.operation_gate(&key).await;
        let guard = Arc::clone(&gate.lock).lock_owned().await;
        let storage = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            *gate
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            gate.attempt.fetch_add(1, Ordering::AcqRel);
            storage.cache.write().await.remove(&key);
            storage.bump_generation();
            let address = storage.broker_address(&key.0, &key.1);
            let request = BrokerRequest {
                op: "delete",
                service: Some(&address.service),
                account: Some(&address.account),
                payload: None,
            };
            let response = storage.invoke_broker(&request).await?;
            require_success("delete", &response)
        })
        .await
        .map_err(|error| SecureStorageError::BackendUnavailable(error.to_string()))?
    }

    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        let service_name = self.keychain_service_name(service);
        let request = BrokerRequest {
            op: "list",
            service: Some(&service_name),
            account: None,
            payload: None,
        };
        let response = self.invoke_broker(&request).await?;
        require_success("list", &response)?;
        Ok(response.accounts.unwrap_or_default())
    }

    fn is_encrypted(&self) -> bool {
        true
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::MacOsKeychain
    }
}

fn require_success(action: &str, response: &BrokerResponse) -> Result<(), SecureStorageError> {
    let Some(version) = response.protocol_version else {
        return Err(SecureStorageError::BackendUnavailable(format!(
            "credential broker omitted its protocol version during {action}; upgrade LingXi"
        )));
    };
    if version != BROKER_PROTOCOL_VERSION {
        return Err(SecureStorageError::BackendUnavailable(format!(
            "credential broker protocol mismatch during {action}: expected {BROKER_PROTOCOL_VERSION}, got {version}; upgrade LingXi"
        )));
    }
    if response.ok {
        return Ok(());
    }
    let message = response
        .error
        .clone()
        .unwrap_or_else(|| format!("macOS credential broker {action} failed"));
    match response.error_kind.as_deref() {
        Some("permission") => Err(SecureStorageError::PermissionDenied(message)),
        Some("locked") | Some("unavailable") => {
            Err(SecureStorageError::BackendUnavailable(message))
        }
        Some("invalid_request") | Some("internal") | _ => Err(SecureStorageError::Io(message)),
    }
}

fn broker_channel() -> &'static str {
    if cfg!(debug_assertions) {
        "development"
    } else {
        "production"
    }
}

fn encode_storage_payload(data: &SecureStorageData) -> Result<String, SecureStorageError> {
    serde_json::to_vec(data)
        .map(|payload| base64::engine::general_purpose::STANDARD.encode(payload))
        .map_err(|error| SecureStorageError::Io(format!("serialize storage payload: {error}")))
}

fn decode_storage_payload(payload: &str) -> Result<SecureStorageData, SecureStorageError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|error| {
            SecureStorageError::Io(format!("decode credential broker payload: {error}"))
        })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        SecureStorageError::Io(format!("deserialize credential broker payload: {error}"))
    })
}

fn resolve_broker_client() -> Result<PathBuf, SecureStorageError> {
    let current = std::env::current_exe().map_err(|error| {
        SecureStorageError::BackendUnavailable(format!(
            "credential broker client location is unavailable: {error}"
        ))
    })?;
    resolve_broker_client_from_executable(&current).ok_or_else(|| {
        SecureStorageError::BackendUnavailable(
            "credential broker client location is unavailable; install a signed LingXi package"
                .to_string(),
        )
    })
}

fn resolve_broker_client_from_executable(current: &std::path::Path) -> Option<PathBuf> {
    candidate_client_paths(current)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn candidate_client_paths(current: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(parent) = current.parent() {
        out.push(
            parent
                .join(BROKER_RESOURCE_DIRNAME)
                .join("bin")
                .join(BROKER_CLIENT_EXECUTABLE),
        );
        if let Some(grandparent) = parent.parent() {
            out.push(
                grandparent
                    .join(BROKER_RESOURCE_DIRNAME)
                    .join("bin")
                    .join(BROKER_CLIENT_EXECUTABLE),
            );
            if let Some(great) = grandparent.parent() {
                out.push(
                    great
                        .join(BROKER_RESOURCE_DIRNAME)
                        .join("bin")
                        .join(BROKER_CLIENT_EXECUTABLE),
                );
            }
        }
    }
    dedupe_paths(out)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.iter().any(|existing| existing == &path) {
            unique.push(path);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{SecretKindDto, SecureStorageMetadata};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    fn sample_data(value: &[u8]) -> SecureStorageData {
        SecureStorageData::new(
            value.to_vec(),
            SecureStorageMetadata {
                created_at: SystemTime::UNIX_EPOCH,
                last_accessed: None,
                kind: SecretKindDto("generic_api_key".to_string()),
            },
        )
    }

    fn write_script(script: &str) -> PathBuf {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.into_path().join("lingxi-credential-client");
        fs::write(&path, script).expect("write script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }

    fn gated_broker() -> (tempfile::TempDir, Arc<MacOsKeychainStorage>) {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("broker");
        let program = r#"#!/usr/bin/env python3
import json, pathlib, sys, time
root = pathlib.Path(ROOT)
req = json.load(sys.stdin)
op = req['op']
with (root / 'calls').open('a') as f: f.write(op + '\n')
if (root / (op + '-block')).exists():
    (root / (op + '-entered')).touch()
    while not (root / (op + '-release')).exists(): time.sleep(.005)
state = root / req.get('account', 'key')
reply = {'ok': True, 'protocol_version': 1}
if (root / 'fail').exists(): reply = {'ok': False, 'protocol_version': 1, 'error_kind': 'unavailable', 'error': 'fixture offline'}
elif op == 'store': state.write_text(req['payload'])
elif op == 'delete': state.unlink(missing_ok=True)
elif op == 'retrieve' and state.exists(): reply['payload'] = state.read_text()
json.dump(reply, sys.stdout)
"#.replace("ROOT", &serde_json::to_string(root.path().to_str().unwrap()).unwrap());
        fs::write(&script, program).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();
        let storage = Arc::new(MacOsKeychainStorage::with_broker_for_test(
            root.path().to_owned(),
            root.path().to_owned(),
            script,
        ));
        (root, storage)
    }

    async fn broker_entered(root: &std::path::Path, operation: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !root.join(format!("{operation}-entered")).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn authoritative_reads_bypass_hot_values_and_evict_missing_or_failed_values() {
        let (root, first) = gated_broker();
        let second = MacOsKeychainStorage::with_broker_for_test(
            root.path().to_owned(),
            root.path().to_owned(),
            first.broker_client.clone(),
        );
        first
            .store("fixture", "key", sample_data(b"old"))
            .await
            .unwrap();
        second
            .store("fixture", "key", sample_data(b"new"))
            .await
            .unwrap();
        assert_eq!(
            first
                .retrieve("fixture", "key")
                .await
                .unwrap()
                .unwrap()
                .expose_secret_bytes(),
            b"old"
        );
        assert_eq!(
            first
                .retrieve_uncached("fixture", "key")
                .await
                .unwrap()
                .unwrap()
                .expose_secret_bytes(),
            b"new"
        );
        second.delete("fixture", "key").await.unwrap();
        assert!(first
            .retrieve_uncached("fixture", "key")
            .await
            .unwrap()
            .is_none());
        assert!(first.retrieve("fixture", "key").await.unwrap().is_none());
        second
            .store("fixture", "key", sample_data(b"recovered"))
            .await
            .unwrap();
        assert!(first
            .retrieve_uncached("fixture", "key")
            .await
            .unwrap()
            .is_some());
        fs::write(root.path().join("fail"), "").unwrap();
        assert!(first.retrieve_uncached("fixture", "key").await.is_err());
        fs::remove_file(root.path().join("fail")).unwrap();
        second.delete("fixture", "key").await.unwrap();
        assert!(
            first.retrieve("fixture", "key").await.unwrap().is_none(),
            "an authoritative error must evict its previous positive cache entry"
        );
    }

    #[tokio::test]
    async fn blocked_stdin_is_bounded_after_the_mutation_caller_is_cancelled() {
        let (root, mut storage) = gated_broker();
        let inner = Arc::get_mut(&mut storage).unwrap();
        inner.broker_timeout = std::time::Duration::from_millis(500);
        fs::write(
            &inner.broker_client,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\n: > '{}'\nexec sleep 5\n",
                root.path().join("broker-pid").display(),
                root.path().join("store-entered").display()
            ),
        )
        .unwrap();
        let data = sample_data(&vec![0; 30000]);
        let encoded = encode_storage_payload(&data).unwrap();
        let address = storage.broker_address("fixture", "key");
        let request = serde_json::to_vec(&BrokerRequest {
            op: "store",
            service: Some(&address.service),
            account: Some(&address.account),
            payload: Some(encoded.clone()),
        })
        .unwrap();
        eprintln!(
            "synthetic blocked-stdin fixture: encoded_bytes={} request_bytes={} limit_bytes={}",
            encoded.len(),
            request.len(),
            MAX_BROKER_REQUEST_BYTES
        );
        assert!(request.len() < MAX_BROKER_REQUEST_BYTES);
        let clone = storage.clone();
        let started = Instant::now();
        let mut mutation = tokio::spawn(async move { clone.store("fixture", "key", data).await });
        tokio::select! {
            biased;
            _ = broker_entered(root.path(), "store") => {
                eprintln!(
                    "synthetic broker marker observed after {:?}; broker_pid={:?}",
                    started.elapsed(),
                    fs::read_to_string(root.path().join("broker-pid")).ok()
                );
            }
            result = &mut mutation => {
                panic!(
                    "synthetic store finished before broker marker: result={result:?}, elapsed={:?}, marker_exists={}, broker_pid={:?}",
                    started.elapsed(),
                    root.path().join("store-entered").exists(),
                    fs::read_to_string(root.path().join("broker-pid")).ok()
                );
            }
        }
        mutation.abort();
        let _ = mutation.await;
        assert!(tokio::time::timeout(
            std::time::Duration::from_secs(2),
            storage.retrieve("fixture", "key")
        )
        .await
        .unwrap()
        .is_err());
    }

    #[tokio::test]
    async fn cancelled_delete_keeps_new_readers_behind_the_owned_mutation() {
        let (root, storage) = gated_broker();
        storage
            .store("fixture", "key", sample_data(b"old"))
            .await
            .unwrap();
        fs::write(root.path().join("delete-block"), "").unwrap();
        let clone = storage.clone();
        let deletion = tokio::spawn(async move { clone.delete("fixture", "key").await });
        broker_entered(root.path(), "delete").await;
        deletion.abort();
        let _ = deletion.await;
        let mut reading = Box::pin(storage.retrieve("fixture", "key"));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(reading.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        fs::write(root.path().join("delete-release"), "").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), reading)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn queued_stores_publish_in_their_mutation_order() {
        let (root, storage) = gated_broker();
        fs::write(root.path().join("store-block"), "").unwrap();
        let clone = storage.clone();
        let first =
            tokio::spawn(async move { clone.store("fixture", "key", sample_data(b"first")).await });
        broker_entered(root.path(), "store").await;
        let mut second = Box::pin(storage.store("fixture", "key", sample_data(b"second")));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(second.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        fs::write(root.path().join("store-release"), "").unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap();
        let value = storage.retrieve("fixture", "key").await.unwrap().unwrap();
        assert_eq!(
            encode_storage_payload(&value).unwrap(),
            encode_storage_payload(&sample_data(b"second")).unwrap()
        );
    }

    #[tokio::test]
    async fn cancelled_read_leader_does_not_poison_followers() {
        let (root, storage) = gated_broker();
        fs::write(root.path().join("retrieve-block"), "").unwrap();
        let first_storage = storage.clone();
        let first = tokio::spawn(async move { first_storage.retrieve("fixture", "key").await });
        broker_entered(root.path(), "retrieve").await;
        let follower_storage = storage.clone();
        let follower =
            tokio::spawn(async move { follower_storage.retrieve("fixture", "key").await });
        first.abort();
        let _ = first.await;
        fs::write(root.path().join("retrieve-release"), "").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), follower)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(storage.retrieve("fixture", "key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_missing_and_failed_reads_share_only_their_attempt() {
        for fail in [false, true] {
            let (root, storage) = gated_broker();
            let _retained_gate = storage
                .operation_gate(&("fixture".into(), "key".into()))
                .await;
            fs::write(root.path().join("retrieve-block"), "").unwrap();
            if fail {
                fs::write(root.path().join("fail"), "").unwrap();
            }
            let leader_storage = storage.clone();
            let leader =
                tokio::spawn(async move { leader_storage.retrieve("fixture", "key").await });
            broker_entered(root.path(), "retrieve").await;
            let mut follower = Box::pin(storage.retrieve("fixture", "key"));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(follower.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            fs::write(root.path().join("retrieve-release"), "").unwrap();
            let first = leader.await.unwrap();
            let second = follower.await;
            assert_eq!(first.is_err(), fail);
            assert_eq!(second.is_err(), fail);
            assert_eq!(
                fs::read_to_string(root.path().join("calls"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            let _ = fs::remove_file(root.path().join("fail"));
            fs::write(
                root.path().join("key"),
                encode_storage_payload(&sample_data(b"recovered")).unwrap(),
            )
            .unwrap();
            assert!(storage.retrieve("fixture", "key").await.unwrap().is_some());
            assert_eq!(
                fs::read_to_string(root.path().join("calls"))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn cancelled_mutation_keeps_its_gate_until_publication() {
        let (root, storage) = gated_broker();
        fs::write(root.path().join("store-block"), "").unwrap();
        let storing = storage.clone();
        let first =
            tokio::spawn(async move { storing.store("fixture", "key", sample_data(b"new")).await });
        broker_entered(root.path(), "store").await;
        first.abort();
        let _ = first.await;
        let mut later = Box::pin(storage.retrieve("fixture", "key"));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(later.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        fs::write(root.path().join("store-release"), "").unwrap();
        let value = tokio::time::timeout(std::time::Duration::from_secs(5), later)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            encode_storage_payload(&value).unwrap(),
            encode_storage_payload(&sample_data(b"new")).unwrap()
        );
        storage.delete("fixture", "key").await.unwrap();
        assert!(storage.retrieve("fixture", "key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_after_an_old_read_fences_new_reads_and_preserves_other_keys() {
        let (root, storage) = gated_broker();
        fs::write(
            root.path().join("key"),
            encode_storage_payload(&sample_data(b"old")).unwrap(),
        )
        .unwrap();
        fs::write(root.path().join("retrieve-block"), "").unwrap();
        let reading = storage.clone();
        let old = tokio::spawn(async move { reading.retrieve("fixture", "key").await });
        broker_entered(root.path(), "retrieve").await;
        let mut deletion = Box::pin(storage.delete("fixture", "key"));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(deletion.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let mut new_read = Box::pin(storage.retrieve("fixture", "key"));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(new_read.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        // A different key owns a different gate, even while this read is blocked.
        storage
            .store("fixture", "other", sample_data(b"other"))
            .await
            .unwrap();
        fs::write(root.path().join("retrieve-release"), "").unwrap();
        assert!(old.await.unwrap().unwrap().is_some());
        deletion.await.unwrap();
        assert!(new_read.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_retrieve_contains_delete_and_list_roundtrip() {
        let payload = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&sample_data(b"secret")).expect("serialize payload"));
        let storage = MacOsKeychainStorage::with_broker_for_test(
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/Users/x/.lingxi"),
            write_script(&format!(
                r#"#!/bin/sh
set -eu
payload=$(cat)
case "$payload" in
  *'"op":"store"'*) printf '{{"ok":true,"protocol_version":1}}' ;;
  *'"op":"retrieve"'*) printf '{{"ok":true,"protocol_version":1,"payload":"{payload}"}}' ;;
  *'"op":"contains"'*) printf '{{"ok":true,"protocol_version":1,"present":true}}' ;;
  *'"op":"delete"'*) printf '{{"ok":true,"protocol_version":1}}' ;;
  *'"op":"list"'*) printf '{{"ok":true,"protocol_version":1,"accounts":["acct.11111111-1111-4111-8111-111111111111"]}}' ;;
  *) printf '{{"ok":false,"protocol_version":1,"error_kind":"invalid_request","error":"bad request"}}' ;;
esac
"#,
            )),
        );
        storage
            .store(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111",
                sample_data(b"secret"),
            )
            .await
            .expect("store");
        let retrieved = storage
            .retrieve(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111",
            )
            .await
            .expect("retrieve")
            .expect("present");
        assert_eq!(retrieved.expose_secret_bytes(), b"secret");
        assert!(storage
            .contains(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111"
            )
            .await
            .expect("contains"));
        assert_eq!(
            storage
                .list("provider-accounts.fixture")
                .await
                .expect("list"),
            vec!["acct.11111111-1111-4111-8111-111111111111".to_string()]
        );
        storage
            .delete(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111",
            )
            .await
            .expect("delete");
    }

    #[tokio::test]
    async fn inflight_retrieve_is_deduplicated() {
        let payload = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&sample_data(b"cached")).expect("serialize payload"));
        let temp = tempfile::tempdir().expect("tempdir");
        let counter = temp.path().join("calls");
        let script = temp.path().join("lingxi-credential-client");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
set -eu
printf x >> {}
sleep 0.05
printf '{{"ok":true,"protocol_version":1,"payload":"{payload}"}}'
"#,
                counter.display(),
            ),
        )
        .expect("write script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("chmod");
        let storage = MacOsKeychainStorage::with_broker_for_test(
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/Users/x/.lingxi"),
            script,
        );
        let (first, second) = tokio::join!(
            storage.retrieve(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111"
            ),
            storage.retrieve(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111"
            ),
        );
        assert!(first.expect("first").is_some());
        assert!(second.expect("second").is_some());
        assert_eq!(fs::read(&counter).expect("read counter").len(), 1);
    }

    #[tokio::test]
    async fn failed_retrieve_clears_inflight_state_for_the_next_attempt() {
        let storage = MacOsKeychainStorage::with_broker_for_test(
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/Users/x/.lingxi"),
            write_script(
                r#"#!/bin/sh
set -eu
cat >/dev/null
printf '{"ok":false,"protocol_version":1,"error_kind":"unavailable","error":"locked"}'
"#,
            ),
        );

        assert!(storage
            .retrieve(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111"
            )
            .await
            .is_err());
        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            storage.retrieve(
                "provider-accounts.fixture",
                "acct.11111111-1111-4111-8111-111111111111",
            ),
        )
        .await
        .expect("a failed lookup must not leave later reads waiting forever");
        assert!(retry.is_err());
    }

    #[test]
    fn broker_client_resolution_skips_missing_npm_layout_candidates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let executable = temp.path().join("vendor/aarch64-apple-darwin/bin/lingxi");
        let client = temp
            .path()
            .join("vendor/aarch64-apple-darwin/credential-broker/bin/lingxi-credential-client");
        fs::create_dir_all(executable.parent().expect("binary parent")).expect("binary dir");
        fs::create_dir_all(client.parent().expect("client parent")).expect("client dir");
        fs::write(&client, "signed helper placeholder").expect("client");

        assert_eq!(
            resolve_broker_client_from_executable(&executable),
            Some(client)
        );
    }

    #[test]
    fn broker_client_resolution_skips_missing_python_layout_candidates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let executable = temp.path().join("lingxi_cli_bin/bin/lingxi");
        let client = temp
            .path()
            .join("lingxi_cli_bin/credential-broker/bin/lingxi-credential-client");
        fs::create_dir_all(executable.parent().expect("binary parent")).expect("binary dir");
        fs::create_dir_all(client.parent().expect("client parent")).expect("client dir");
        fs::write(&client, "signed helper placeholder").expect("client");

        assert_eq!(
            resolve_broker_client_from_executable(&executable),
            Some(client)
        );
    }

    #[tokio::test]
    async fn missing_broker_fails_closed() {
        let storage = MacOsKeychainStorage::with_broker_for_test(
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/definitely-missing/lingxi-credential-client"),
        );
        assert!(matches!(
            storage
                .retrieve(
                    "provider-accounts.fixture",
                    "acct.11111111-1111-4111-8111-111111111111"
                )
                .await,
            Err(SecureStorageError::BackendUnavailable(_)) | Err(SecureStorageError::Io(_))
        ));
    }

    #[test]
    fn service_name_default_dir_has_no_dir_hash() {
        let storage = MacOsKeychainStorage::with_broker_for_test(
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/tmp/lingxi-credential-client"),
        );
        assert_eq!(
            storage.keychain_service_name("-credentials"),
            "com.lingxi.secure-storage.v1.development.-credentials"
        );
        assert_eq!(
            storage.keychain_service_name(""),
            "com.lingxi.secure-storage.v1.development.default"
        );
    }

    #[test]
    fn responses_without_a_protocol_version_are_rejected() {
        let response = BrokerResponse {
            ok: true,
            present: None,
            payload: None,
            accounts: None,
            protocol_version: None,
            build_version: None,
            error_kind: None,
            error: None,
        };
        assert!(matches!(
            require_success("health", &response),
            Err(SecureStorageError::BackendUnavailable(_))
        ));
    }
}
