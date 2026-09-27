//! Shared test doubles for the OAuth crate's unit tests.
//!
//! All seams the OAuth flow touches — HTTP transport, wall-clock, secure
//! storage, and the background spawner — are mocked here so the token
//! exchange, refresh, and login flows run fully offline (no network, no
//! browser, no real keychain).

use async_trait::async_trait;
use platform_api::http::SseStream;
use platform_api::{
    BackgroundTaskHandle, Clock, HttpError, HttpTransport, RuntimeError, RuntimeSpawner,
    SecureStorage, SecureStorageBackend, SecureStorageError,
};
use protocol::{HttpRequest, HttpResponse, SecureStorageData};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Serializes tests that WRITE the process-global subscription cache
/// (`platform_api::subscription::set_current_subscription`) so one test's write
/// can't interleave with another's assertion. Poison-tolerant.
pub static SUBSCRIPTION_CACHE_LOCK: Mutex<()> = Mutex::new(());

/// One canned response keyed loosely by a URL substring + method.
#[derive(Clone)]
pub struct Canned {
    pub status: u16,
    pub body: String,
}

/// HTTP transport that records every request and replies with a canned
/// response chosen by matching a substring of the request URL. Falls back to
/// the first registered response when nothing matches (single-endpoint tests).
pub struct MockHttp {
    routes: Vec<(String, Canned)>,
    pub requests: Mutex<Vec<HttpRequest>>,
    pub calls: AtomicU64,
}

impl MockHttp {
    /// Build a transport from `(url_substring, response)` routes.
    pub fn new(routes: Vec<(&str, Canned)>) -> Arc<Self> {
        Arc::new(Self {
            routes: routes
                .into_iter()
                .map(|(u, c)| (u.to_string(), c))
                .collect(),
            requests: Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
        })
    }

    /// Number of `request` invocations observed.
    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Clone of the last request body observed (for body assertions).
    pub fn last_request(&self) -> Option<HttpRequest> {
        self.requests.lock().unwrap().last().cloned()
    }
}

#[async_trait]
impl HttpTransport for MockHttp {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(req.clone());
        let canned = self
            .routes
            .iter()
            .find(|(u, _)| req.url.contains(u.as_str()))
            .map(|(_, c)| c.clone())
            .or_else(|| self.routes.first().map(|(_, c)| c.clone()))
            .ok_or_else(|| HttpError::InvalidRequest("no canned route".into()))?;
        Ok(HttpResponse {
            status: canned.status,
            headers: vec![],
            body: canned.body,
            body_bytes: Vec::new(),
        })
    }

    async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
        Err(HttpError::InvalidRequest("sse unsupported in mock".into()))
    }
}

/// Clock returning a fixed instant; `set` advances it for proactive tests.
pub struct TestClock {
    secs: AtomicU64,
}

impl TestClock {
    pub fn new(secs: u64) -> Arc<Self> {
        Arc::new(Self {
            secs: AtomicU64::new(secs),
        })
    }
    pub fn set(&self, secs: u64) {
        self.secs.store(secs, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.secs.load(Ordering::SeqCst))
    }
}

/// In-memory secure storage map.
#[derive(Default)]
pub struct MemStorage {
    map: Mutex<HashMap<(String, String), SecureStorageData>>,
}

impl MemStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    /// Number of entries currently stored under `service`.
    pub fn count(&self, service: &str) -> usize {
        self.map
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| s == service)
            .count()
    }
}

#[async_trait]
impl SecureStorage for MemStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .insert((service.into(), account.into()), data);
        Ok(())
    }
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&(service.into(), account.into()))
            .cloned())
    }
    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .remove(&(service.into(), account.into()));
        Ok(())
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, a)| a.clone())
            .collect())
    }
    fn is_encrypted(&self) -> bool {
        false
    }
    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::PlainText
    }
}

/// Spawner whose `sleep` resolves immediately (no real delay) so proactive
/// timer loops can be driven deterministically. Tracks spawn + cancel.
pub struct InstantSpawner {
    next_id: AtomicU64,
    pub canceled: Mutex<Vec<u64>>,
}

impl InstantSpawner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            canceled: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl RuntimeSpawner for InstantSpawner {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(task);
        Ok(BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: id,
        })
    }
    async fn sleep(&self, _duration: Duration) {
        // Resolve immediately; yield once so other tasks can make progress.
        tokio::task::yield_now().await;
    }
    async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        self.canceled.lock().unwrap().push(handle.task_id);
        Ok(())
    }
}

/// Build a `CredentialManager` over an in-memory store + fixed clock.
pub fn mem_credential_manager(
    storage: Arc<MemStorage>,
    clock: Arc<dyn Clock>,
) -> Arc<secret::CredentialManager> {
    // Re-use a never-called HTTP mock for the credential manager's http field.
    let http = MockHttp::new(vec![(
        "__never__",
        Canned {
            status: 500,
            body: String::new(),
        },
    )]);
    Arc::new(secret::CredentialManager::new(
        storage as Arc<dyn SecureStorage>,
        clock,
        http as Arc<dyn HttpTransport>,
    ))
}
