//! Exercise logical deletion and unavailable native stores through real account consumers.
#![cfg(all(unix, feature = "desktop"))]
use async_trait::async_trait;
use lingxi_core::host::{
    InMemorySecureStorage, SecureStorage, SecureStorageBackend, SecureStorageError,
};
use lingxi_core::types::{SecretKindDto, SecureStorageData, SecureStorageMetadata};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::SystemTime;

struct ToggleNative {
    owner: SecureStorageBackend,
    inner: InMemorySecureStorage,
    unavailable: AtomicBool,
}
impl ToggleNative {
    fn available(&self) -> Result<(), SecureStorageError> {
        if self.unavailable.load(Ordering::SeqCst) {
            Err(SecureStorageError::BackendUnavailable(
                "synthetic native lock".into(),
            ))
        } else {
            Ok(())
        }
    }
}
#[async_trait]
impl SecureStorage for ToggleNative {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        self.available()?;
        self.inner.store(service, account, data).await
    }
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.available()?;
        self.inner.retrieve(service, account).await
    }
    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.available()?;
        self.inner.delete(service, account).await
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        self.available()?;
        self.inner.list(service).await
    }
    fn is_encrypted(&self) -> bool {
        true
    }
    fn backend(&self) -> SecureStorageBackend {
        self.owner
    }
}
fn payload(text: &str) -> SecureStorageData {
    SecureStorageData::new(
        text.as_bytes().to_vec(),
        SecureStorageMetadata {
            created_at: SystemTime::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("probe".into()),
        },
    )
}
async fn tombstone_case() {
    let root = tempfile::tempdir().unwrap();
    let primary = Arc::new(ToggleNative {
        owner: SecureStorageBackend::LinuxLibsecret,
        inner: InMemorySecureStorage::new(),
        unavailable: AtomicBool::new(false),
    });
    let fallback = Arc::new(InMemorySecureStorage::new());
    let storage = platform_posix::secure_storage::factory::runtime_fallback_storage_for_test(
        Ok(primary.clone()),
        SecureStorageBackend::LinuxLibsecret,
        fallback,
        root.path().join("locks"),
    );
    storage
        .store("fixture", "a", payload("old-native"))
        .await
        .unwrap();
    storage
        .store("fixture", "other", payload("other-native"))
        .await
        .unwrap();
    assert!(storage.contains("fixture", "a").await.unwrap());
    primary.unavailable.store(true, Ordering::SeqCst);
    storage.delete("fixture", "a").await.unwrap();
    let deleted_read = storage.retrieve("fixture", "a").await.unwrap();
    let deleted_contains = storage.contains("fixture", "a").await.unwrap();
    assert!(!deleted_contains);
    assert!(deleted_read.is_none());
    primary.unavailable.store(false, Ordering::SeqCst);
    let recovered_read = storage.retrieve_uncached("fixture", "a").await.unwrap();
    let recovered_contains = storage.contains("fixture", "a").await.unwrap();
    assert!(!recovered_contains);
    let listed = storage.list("fixture").await.unwrap();
    assert!(recovered_read.is_none());
    assert!(!listed.iter().any(|account| account == "a"));
    assert_eq!(
        storage
            .retrieve("fixture", "other")
            .await
            .unwrap()
            .unwrap()
            .expose_secret_bytes(),
        b"other-native"
    );
    storage
        .store("fixture", "a", payload("new-native"))
        .await
        .unwrap();
    assert!(storage.contains("fixture", "a").await.unwrap());
    assert_eq!(
        storage
            .retrieve("fixture", "a")
            .await
            .unwrap()
            .unwrap()
            .expose_secret_bytes(),
        b"new-native"
    );
}
async fn native_account_logout_case() {
    native_account_logout_for_owner(SecureStorageBackend::LinuxLibsecret).await;
    native_account_logout_for_owner(SecureStorageBackend::WindowsCredVault).await;
}
async fn native_account_logout_for_owner(owner: SecureStorageBackend) {
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, ProviderAccountAction, ProviderAccountCommand,
    };
    let root = tempfile::tempdir().unwrap();
    let primary = Arc::new(ToggleNative {
        owner,
        inner: InMemorySecureStorage::new(),
        unavailable: AtomicBool::new(false),
    });
    let storage = platform_posix::secure_storage::factory::runtime_fallback_storage_for_test(
        Ok(primary.clone()),
        owner,
        Arc::new(InMemorySecureStorage::new()),
        root.path().join("locks"),
    );
    let credentials = Arc::new(secret::CredentialManager::new(
        storage,
        Arc::new(platform_posix::PosixClock::new()),
        Arc::new(platform_posix::PosixHttp::new()),
    ));
    let store = harness_runtime::provider_accounts::ProviderAccountStore::new(
        root.path().into(),
        "native_logout_fixture".into(),
        credentials.clone(),
    );
    let added = store
        .execute(
            ProviderAccountCommand {
                expected_revision: 0,
                operation_id: None,
                action: ProviderAccountAction::Add {
                    account_id: None,
                    provider_id: "openai".into(),
                    display_name: "Fixture".into(),
                    execution_binding_id: "openai".into(),
                    auth_kind: AccountAuthKind::ApiKey,
                },
            },
            Some("synthetic-api-key".into()),
        )
        .await
        .unwrap();
    let account = added.accounts[0].clone();
    assert_eq!(
        store.snapshot().await.unwrap().health[0].status,
        lingxi_core::host::provider_accounts::AccountHealthStatus::Ready
    );
    primary.unavailable.store(true, Ordering::SeqCst);
    assert_eq!(
        store.snapshot().await.unwrap().health[0].status,
        lingxi_core::host::provider_accounts::AccountHealthStatus::StorageUnavailable,
    );
    primary.unavailable.store(false, Ordering::SeqCst);
    assert_eq!(
        store.snapshot().await.unwrap().health[0].status,
        lingxi_core::host::provider_accounts::AccountHealthStatus::Ready,
    );
    primary.unavailable.store(true, Ordering::SeqCst);
    store
        .execute(
            ProviderAccountCommand {
                expected_revision: added.config_revision,
                operation_id: None,
                action: ProviderAccountAction::Logout {
                    account_id: account.account_id.clone(),
                },
            },
            None,
        )
        .await
        .unwrap();
    for recovered in [false, true] {
        primary.unavailable.store(!recovered, Ordering::SeqCst);
        let snapshot = store.snapshot().await.unwrap();
        let account = &snapshot.accounts[0];
        let health = snapshot
            .health
            .iter()
            .find(|row| row.account_id == account.account_id)
            .unwrap();
        let availability = provider_config::compute_availability_with_accounts(
            &credentials,
            &[provider_config::CredentialSource {
                provider_id: llm_runtime::ProviderId::OpenAI,
                profile_name: format!("account.{}", account.account_id),
                credential_id: account.credential_ref.secret_ref.clone(),
                env_var: None,
                kind: provider_config::CredentialKind::ApiKey,
            }],
            &snapshot,
        )
        .await;
        assert!(credentials
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            health.status,
            lingxi_core::host::provider_accounts::AccountHealthStatus::NeedsAuth
        );
        assert!(!availability[0].available);
    }
}
#[cfg(unix)]
async fn linux_lookup_case() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("secret-tool");
    let saved_path = std::env::var_os("PATH");
    struct RestorePath(Option<std::ffi::OsString>);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            match &self.0 {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
    let _restore = RestorePath(saved_path);
    // This standalone process's executable lookup is confined to the fixture.
    std::env::set_var("PATH", root.path());
    for (label, script, expected_error) in [
        ("missing", "#!/bin/sh\nexit 1\n", false),
        ("terminated", "#!/bin/sh\nkill -TERM $$\n", true),
        ("abnormal_exit", "#!/bin/sh\nexit 2\n", true),
        (
            "daemon_error",
            "#!/bin/sh\nprintf 'Cannot connect to the Secret Service daemon\\n' >&2\nexit 1\n",
            true,
        ),
    ] {
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let storage = platform_posix::secure_storage::LinuxSecretStorage::new(
            "fixture-user".into(),
            root.path().into(),
            root.path().into(),
            String::new(),
        )
        .unwrap();
        let result = storage.retrieve_uncached("fixture", "a").await;
        let actual_error = result.is_err();
        let actual_missing = matches!(result, Ok(None));
        assert_eq!(actual_error, expected_error, "{label}");
        assert_eq!(actual_missing, !expected_error, "{label}");
    }
}
#[cfg(unix)]
async fn linux_account_daemon_case() {
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, ProviderAccountAction, ProviderAccountCommand,
    };
    use llm_runtime::CredentialProvider;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("secret-tool");
    let slot = root.path().join("synthetic-native-slot");
    let fail = root.path().join("synthetic-daemon-unavailable");
    let quote =
        |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    std::fs::write(&binary,format!("#!/bin/sh\ncase \"$1\" in\nstore) /bin/cat > {}; exit 0;;\nlookup) if [ -e {} ]; then printf 'Cannot connect to the Secret Service daemon\\n' >&2; exit 1; fi; if [ -f {} ]; then /bin/cat {}; exit 0; fi; exit 1;;\nclear) /bin/rm -f {}; exit 0;;\n*) exit 1;;\nesac\n",quote(&slot),quote(&fail),quote(&slot),quote(&slot),quote(&slot))).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let saved_path = std::env::var_os("PATH");
    struct RestorePath(Option<std::ffi::OsString>);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            match &self.0 {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
    let _restore = RestorePath(saved_path);
    std::env::set_var("PATH", root.path());
    let primary = Arc::new(
        platform_posix::secure_storage::LinuxSecretStorage::new(
            "fixture-user".into(),
            root.path().into(),
            root.path().into(),
            String::new(),
        )
        .unwrap(),
    );
    let storage = platform_posix::secure_storage::factory::runtime_fallback_storage_for_test(
        Ok(primary),
        SecureStorageBackend::LinuxLibsecret,
        Arc::new(InMemorySecureStorage::new()),
        root.path().join("locks"),
    );
    let credentials = Arc::new(secret::CredentialManager::new(
        storage,
        Arc::new(platform_posix::PosixClock::new()),
        Arc::new(platform_posix::PosixHttp::new()),
    ));
    let store = harness_runtime::provider_accounts::ProviderAccountStore::new(
        root.path().into(),
        "linux_daemon_fixture".into(),
        credentials.clone(),
    );
    let added = store
        .execute(
            ProviderAccountCommand {
                expected_revision: 0,
                operation_id: None,
                action: ProviderAccountAction::Add {
                    account_id: None,
                    provider_id: "openai".into(),
                    display_name: "Fixture".into(),
                    execution_binding_id: "openai".into(),
                    auth_kind: AccountAuthKind::ApiKey,
                },
            },
            Some("synthetic-api-key".into()),
        )
        .await
        .unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().health[0].status,
        lingxi_core::host::provider_accounts::AccountHealthStatus::Ready
    );
    let saved_slot = std::fs::read(&slot).unwrap();
    std::fs::write(&fail, b"unavailable").unwrap();
    let snapshot = store.snapshot().await.unwrap();
    let account = &snapshot.accounts[0];
    let provider = provider_config::MultiCredentialProvider::new(credentials)
        .with_accounts(vec![account.clone()])
        .with_account_version_root(root.path().into());
    let mut scope = llm_runtime::CredentialScope::new(
        llm_runtime::ProviderId::OpenAI,
        format!("account.{}", account.account_id),
    )
    .with_credential_id(account.credential_ref.secret_ref.clone());
    scope.expected_identity_generation = Some(account.identity_generation);
    let load = provider.load_versioned(&scope).await;
    let health = snapshot
        .health
        .iter()
        .find(|row| row.account_id == account.account_id)
        .unwrap();
    let actual_auth_error = matches!(
        load,
        Err(llm_runtime::VersionedCredentialError {
            error: llm_runtime::LlmError::Authentication { .. },
            ..
        })
    );
    assert_eq!(
        std::fs::read(&slot).unwrap(),
        saved_slot,
        "the grant remains in the native fixture"
    );
    assert!(!actual_auth_error);
    assert_eq!(
        health.status,
        lingxi_core::host::provider_accounts::AccountHealthStatus::StorageUnavailable
    );
    std::fs::remove_file(&fail).unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().health[0].status,
        lingxi_core::host::provider_accounts::AccountHealthStatus::Ready
    );
    assert_eq!(added.accounts.len(), 1);
}

fn isolated(name: &str, work: impl std::future::Future<Output = ()>) {
    const CHILD: &str = "HARNESS_CREDENTIAL_STORAGE_BOUNDARY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(work);
    }
}
#[test]
fn fallback_logical_presence_and_replacement() {
    isolated(
        "fallback_logical_presence_and_replacement",
        tombstone_case(),
    );
}
#[test]
fn logout_is_missing_in_account_health_and_picker_after_native_recovery() {
    isolated(
        "logout_is_missing_in_account_health_and_picker_after_native_recovery",
        native_account_logout_case(),
    );
}
#[test]
fn linux_lookup_distinguishes_missing_from_daemon_failure() {
    isolated(
        "linux_lookup_distinguishes_missing_from_daemon_failure",
        linux_lookup_case(),
    );
}
#[test]
fn linux_daemon_failure_is_storage_unavailable_without_erasing_native_grant() {
    isolated(
        "linux_daemon_failure_is_storage_unavailable_without_erasing_native_grant",
        linux_account_daemon_case(),
    );
}
