use super::*;
use crate::auth::anthropic::testsupport::{Canned, MemStorage, MockHttp, TestClock};
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::SecureStorageData;
use lingxi_llm_client::protocol::LlmError;
use lingxi_llm_client::transport::{HttpRequest, StreamResponse};
use std::sync::atomic::{AtomicBool, Ordering};

const TOKEN_BODY: &str = r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_in":3600,"scope":"user:profile user:inference","account":{"uuid":"test-account","email_address":"account@example.test"},"organization":{"uuid":"test-org"}}"#;

struct RecordingObserver {
    storage: Arc<MemStorage>,
    persisted_counts: Mutex<Vec<usize>>,
}
impl AccountChangeObserver for RecordingObserver {
    fn account_changed(&self) {
        self.persisted_counts
            .lock()
            .unwrap()
            .push(self.storage.count("lingxi"));
    }
}

fn observe(handle: &OAuthHandle, storage: Arc<MemStorage>) -> Arc<RecordingObserver> {
    let observer = Arc::new(RecordingObserver {
        storage,
        persisted_counts: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn AccountChangeObserver> = observer.clone();
    handle.register_account_change_observer(Arc::downgrade(&erased));
    // Re-registering the same owner must not duplicate its notification.
    handle.register_account_change_observer(Arc::downgrade(&erased));
    observer
}

fn transport() -> Arc<MockHttp> {
    MockHttp::new(vec![
        (
            "oauth/token",
            Canned {
                status: 200,
                body: TOKEN_BODY.into(),
            },
        ),
        (
            "/api/oauth/profile",
            Canned {
                status: 500,
                body: "{}".into(),
            },
        ),
    ])
}

fn handle(storage: Arc<dyn SecureStorage>, http: Arc<dyn Transport>) -> OAuthHandle {
    let clock = TestClock::new(1_000);
    let credentials = Arc::new(CredentialManager::new(storage, clock.clone(), transport()));
    OAuthHandle::new(
        ClaudeAiOAuthConfig::default_with_port(0),
        http,
        credentials,
        clock,
    )
    .with_browser_opener(Arc::new(|_| Ok(())))
}

async fn mobile_login(handle: &OAuthHandle) -> Result<LoginInfo, AuthError> {
    handle
        .complete_mobile_browser_login("test-code", "test-verifier", "test-state", "lingxi://oauth")
        .await
}

#[tokio::test]
async fn account_observer_receives_only_fully_persisted_login_and_logout() {
    let storage = MemStorage::new();
    let handle = handle(storage.clone(), transport());
    let observer = observe(&handle, storage.clone());
    let user = mobile_login(&handle).await.unwrap();
    assert_eq!(user.email, "account@example.test");
    assert_eq!(*observer.persisted_counts.lock().unwrap(), [3]);
    assert_eq!(handle.current_user().await, Some(user));
    handle.logout().await.unwrap();
    assert_eq!(*observer.persisted_counts.lock().unwrap(), [3, 0]);
    assert!(handle.current_user().await.is_none());
    drop(observer);
    handle.logout().await.unwrap();
    assert!(handle.account_observers.lock().unwrap().is_empty());
}

struct BlockedProfile {
    inner: Arc<MockHttp>,
    entered: tokio::sync::Notify,
}
#[async_trait]
impl Transport for BlockedProfile {
    async fn send(&self, request: HttpRequest) -> Result<StreamResponse, LlmError> {
        if request.url.contains("/api/oauth/profile") {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.send(request).await
    }
}

#[tokio::test]
async fn account_observer_notifies_before_cancelled_subscription_lookup() {
    let storage = MemStorage::new();
    let http = Arc::new(BlockedProfile {
        inner: transport(),
        entered: tokio::sync::Notify::new(),
    });
    let handle = Arc::new(handle(storage.clone(), http.clone()));
    let observer = observe(&handle, storage);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(("test-code".into(), "test-state".into()))
        .unwrap();
    let task_handle = handle.clone();
    let login = tokio::spawn(async move {
        task_handle
            .login_with_options_and_io(
                OAuthLoginOptions::default(),
                CodeFlowIo {
                    on_url: None,
                    manual_rx: Some(rx),
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), http.entered.notified())
        .await
        .unwrap();
    assert_eq!(*observer.persisted_counts.lock().unwrap(), [3]);
    assert_eq!(
        handle.current_user().await.unwrap().email,
        "account@example.test"
    );
    login.abort();
    assert!(login.await.unwrap_err().is_cancelled());
    assert_eq!(*observer.persisted_counts.lock().unwrap(), [3]);
}

struct FailingStorage {
    inner: Arc<MemStorage>,
    fail_store: AtomicBool,
    fail_delete: AtomicBool,
}
#[async_trait]
impl SecureStorage for FailingStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        if self.fail_store.load(Ordering::SeqCst) {
            return Err(SecureStorageError::Io("test store failed".into()));
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
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(SecureStorageError::Io("test delete failed".into()));
        }
        self.inner.delete(service, account).await
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        self.inner.list(service).await
    }
    fn is_encrypted(&self) -> bool {
        self.inner.is_encrypted()
    }
    fn backend(&self) -> SecureStorageBackend {
        self.inner.backend()
    }
}

#[tokio::test]
async fn account_observer_does_not_notify_failed_storage_mutations() {
    let storage = Arc::new(FailingStorage {
        inner: MemStorage::new(),
        fail_store: AtomicBool::new(true),
        fail_delete: AtomicBool::new(false),
    });
    let handle = handle(storage.clone(), transport());
    let observer = observe(&handle, storage.inner.clone());
    assert!(mobile_login(&handle).await.is_err());
    assert!(observer.persisted_counts.lock().unwrap().is_empty());
    assert_eq!(storage.inner.count("lingxi"), 0);
    storage.fail_store.store(false, Ordering::SeqCst);
    mobile_login(&handle).await.unwrap();
    storage.fail_delete.store(true, Ordering::SeqCst);
    assert!(handle.logout().await.is_err());
    assert_eq!(*observer.persisted_counts.lock().unwrap(), [3]);
    assert_eq!(storage.inner.count("lingxi"), 3);
    storage.fail_delete.store(false, Ordering::SeqCst);
    handle.logout().await.unwrap();
    assert_eq!(*observer.persisted_counts.lock().unwrap(), [3, 0]);
}

#[tokio::test]
async fn token_rotation_and_subscription_updates_do_not_emit_account_change() {
    let storage = MemStorage::new();
    let handle = handle(storage.clone(), transport());
    let observer = observe(&handle, storage);
    handle
        .credentials
        .store_oauth_tokens(
            "rotated-access",
            Some("rotated-refresh"),
            handle.clock.now(),
            Vec::new(),
            "account@example.test",
            "test-org",
        )
        .await
        .unwrap();
    handle
        .credentials
        .update_oauth_subscription(Some("max"), None)
        .await
        .unwrap();
    assert!(observer.persisted_counts.lock().unwrap().is_empty());
}
