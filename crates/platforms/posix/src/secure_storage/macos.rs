//! macOS secure-storage backend routed through the signed LingXi credential
//! client, which forwards bounded requests to the app-bundled XPC broker.

use crate::secure_storage::helpers::KEYCHAIN_CACHE_TTL;
use async_trait::async_trait;
use base64::Engine as _;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::{SecretKindDto, SecureStorageData, SecureStorageMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, RwLock};

type CacheKey = (String, String);
const BROKER_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const BROKER_RESOURCE_DIRNAME: &str = "credential-broker";
const BROKER_CLIENT_EXECUTABLE: &str = "lingxi-credential-client";
const BROKER_PROTOCOL_VERSION: u32 = 1;
const MAX_BROKER_REQUEST_BYTES: usize = 128 * 1024;
const MAX_BROKER_RESPONSE_BYTES: usize = 128 * 1024;
#[derive(Clone, Copy)]
enum PayloadFormat {
    Json,
    Provider(&'static str),
}

struct BrokerAddress {
    service: String,
    account: String,
    payload_format: PayloadFormat,
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

pub struct MacOsKeychainStorage {
    #[allow(dead_code)]
    user: String,
    config_dir: PathBuf,
    default_config_dir: PathBuf,
    oauth_suffix: String,
    broker_client: PathBuf,
    cache: Arc<RwLock<HashMap<CacheKey, CachedEntry>>>,
    generation: Arc<AtomicU64>,
    inflight: Arc<Mutex<HashMap<CacheKey, Arc<InflightLookup>>>>,
}

struct InflightLookup {
    notify: Notify,
    result: Mutex<Option<Result<Option<SecureStorageData>, SecureStorageError>>>,
}

impl MacOsKeychainStorage {
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
        if service == "lingxi" {
            if account == "anthropic-api-key" {
                return BrokerAddress {
                    service: provider_service_name(),
                    account: "anthropic".to_string(),
                    payload_format: PayloadFormat::Provider("anthropic_api_key"),
                };
            }
            if let Some(provider_id) = account.strip_prefix("provider-key-") {
                return BrokerAddress {
                    service: provider_service_name(),
                    account: provider_id.to_string(),
                    payload_format: PayloadFormat::Provider("generic_api_key"),
                };
            }
        }
        BrokerAddress {
            service: self.keychain_service_name(service),
            account: account.to_string(),
            payload_format: PayloadFormat::Json,
        }
    }

    pub(crate) fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn with_broker_for_test(
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
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&payload).await.map_err(|error| {
                SecureStorageError::Io(format!("credential client stdin write: {error}"))
            })?;
            stdin.shutdown().await.map_err(|error| {
                SecureStorageError::Io(format!("credential client stdin shutdown: {error}"))
            })?;
        }
        let output = tokio::time::timeout(BROKER_COMMAND_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| {
                SecureStorageError::BackendUnavailable(
                    "macOS credential client timed out".to_string(),
                )
            })?
            .map_err(|error| SecureStorageError::Io(format!("credential client wait: {error}")))?;
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
        let key = (service.to_string(), account.to_string());
        self.cache.write().await.remove(&key);
        self.bump_generation();
        let address = self.broker_address(service, account);
        let request = BrokerRequest {
            op: "store",
            service: Some(&address.service),
            account: Some(&address.account),
            payload: Some(encode_storage_payload(&data, address.payload_format)?),
        };
        let response = self.invoke_broker(&request).await?;
        require_success("store", &response)?;
        let generation = self.generation.load(Ordering::Acquire);
        self.cache.write().await.insert(
            key,
            CachedEntry {
                data,
                fetched_at: Instant::now(),
                generation,
            },
        );
        Ok(())
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let key = (service.to_string(), account.to_string());
        let now = Instant::now();
        let current_generation = self.generation.load(Ordering::Acquire);
        if let Some(entry) = self.cache.read().await.get(&key).cloned() {
            if entry.generation == current_generation
                && now.duration_since(entry.fetched_at) < KEYCHAIN_CACHE_TTL
            {
                return Ok(Some(entry.data));
            }
        }

        let (lookup, is_leader) = {
            let mut inflight = self.inflight.lock().await;
            if let Some(existing) = inflight.get(&key).cloned() {
                (existing, false)
            } else {
                let lookup = Arc::new(InflightLookup {
                    notify: Notify::new(),
                    result: Mutex::new(None),
                });
                inflight.insert(key.clone(), lookup.clone());
                (lookup, true)
            }
        };
        if !is_leader {
            loop {
                let notified = lookup.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(result) = lookup.result.lock().await.clone() {
                    return result;
                }
                notified.await;
            }
        }

        let address = self.broker_address(service, account);
        let request = BrokerRequest {
            op: "retrieve",
            service: Some(&address.service),
            account: Some(&address.account),
            payload: None,
        };
        let result = match self.invoke_broker(&request).await {
            Ok(response) => {
                async {
                    require_success("retrieve", &response)?;
                    let data = if let Some(payload) = response.payload.as_deref() {
                        Some(decode_storage_payload(payload, address.payload_format)?)
                    } else {
                        None
                    };
                    if let Some(data) = data.clone() {
                        let post_generation = self.generation.load(Ordering::Acquire);
                        if post_generation == current_generation {
                            self.cache.write().await.insert(
                                key.clone(),
                                CachedEntry {
                                    data: data.clone(),
                                    fetched_at: Instant::now(),
                                    generation: post_generation,
                                },
                            );
                        }
                        Ok(Some(data))
                    } else {
                        Ok(None)
                    }
                }
                .await
            }
            Err(error) => Err(error),
        };

        *lookup.result.lock().await = Some(result.clone());
        self.inflight.lock().await.remove(&key);
        lookup.notify.notify_waiters();
        result
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
        let key = (service.to_string(), account.to_string());
        self.cache.write().await.remove(&key);
        self.bump_generation();
        let address = self.broker_address(service, account);
        let request = BrokerRequest {
            op: "delete",
            service: Some(&address.service),
            account: Some(&address.account),
            payload: None,
        };
        let response = self.invoke_broker(&request).await?;
        require_success("delete", &response)
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

fn provider_service_name() -> String {
    if broker_channel() == "production" {
        "com.lingxi.provider-credentials.v1".to_string()
    } else {
        "com.lingxi.provider-credentials.v1.development".to_string()
    }
}

fn encode_storage_payload(
    data: &SecureStorageData,
    payload_format: PayloadFormat,
) -> Result<String, SecureStorageError> {
    match payload_format {
        PayloadFormat::Provider(_) => String::from_utf8(data.expose_secret_bytes().to_vec())
            .map_err(|_| {
                SecureStorageError::Io("provider credential is not valid UTF-8".to_string())
            }),
        PayloadFormat::Json => serde_json::to_vec(data)
            .map(|payload| base64::engine::general_purpose::STANDARD.encode(payload))
            .map_err(|error| SecureStorageError::Io(format!("serialize storage payload: {error}"))),
    }
}

fn decode_storage_payload(
    payload: &str,
    payload_format: PayloadFormat,
) -> Result<SecureStorageData, SecureStorageError> {
    match payload_format {
        PayloadFormat::Provider(kind) => Ok(SecureStorageData::new(
            payload.as_bytes().to_vec(),
            SecureStorageMetadata {
                created_at: std::time::SystemTime::now(),
                last_accessed: Some(std::time::SystemTime::now()),
                kind: SecretKindDto(kind.to_string()),
            },
        )),
        PayloadFormat::Json => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(payload)
                .map_err(|error| {
                    SecureStorageError::Io(format!("decode credential broker payload: {error}"))
                })?;
            serde_json::from_slice(&bytes).map_err(|error| {
                SecureStorageError::Io(format!("deserialize credential broker payload: {error}"))
            })
        }
    }
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
  *'"op":"list"'*) printf '{{"ok":true,"protocol_version":1,"accounts":["provider-key-openai"]}}' ;;
  *) printf '{{"ok":false,"protocol_version":1,"error_kind":"invalid_request","error":"bad request"}}' ;;
esac
"#,
            )),
        );
        storage
            .store("", "provider-key-openai", sample_data(b"secret"))
            .await
            .expect("store");
        let retrieved = storage
            .retrieve("", "provider-key-openai")
            .await
            .expect("retrieve")
            .expect("present");
        assert_eq!(retrieved.expose_secret_bytes(), b"secret");
        assert!(storage
            .contains("", "provider-key-openai")
            .await
            .expect("contains"));
        assert_eq!(
            storage.list("").await.expect("list"),
            vec!["provider-key-openai".to_string()]
        );
        storage
            .delete("", "provider-key-openai")
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
            storage.retrieve("", "provider-key-openai"),
            storage.retrieve("", "provider-key-openai"),
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

        assert!(storage.retrieve("", "provider-key-openai").await.is_err());
        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            storage.retrieve("", "provider-key-openai"),
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
            storage.retrieve("lingxi", "provider-key-openai").await,
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
    fn provider_accounts_share_the_channel_scoped_provider_service() {
        let storage = MacOsKeychainStorage::with_broker_for_test(
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/Users/x/.lingxi"),
            PathBuf::from("/tmp/lingxi-credential-client"),
        );
        let anthropic = storage.broker_address("lingxi", "anthropic-api-key");
        assert_eq!(
            anthropic.service,
            "com.lingxi.provider-credentials.v1.development"
        );
        assert_eq!(anthropic.account, "anthropic");
        let openai = storage.broker_address("lingxi", "provider-key-openai");
        assert_eq!(openai.service, anthropic.service);
        assert_eq!(openai.account, "openai");
        let web = storage.broker_address("lingxi", "provider-key-web:tavily");
        assert_eq!(web.service, anthropic.service);
        assert_eq!(web.account, "web:tavily");
    }

    #[test]
    fn provider_payload_is_sent_as_the_raw_secret() {
        let encoded = encode_storage_payload(
            &sample_data(b"sk-test-secret"),
            PayloadFormat::Provider("generic_api_key"),
        )
        .expect("encode provider secret");
        assert_eq!(encoded, "sk-test-secret");
        let decoded = decode_storage_payload(&encoded, PayloadFormat::Provider("generic_api_key"))
            .expect("decode provider secret");
        assert_eq!(decoded.expose_secret_bytes(), b"sk-test-secret");
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
