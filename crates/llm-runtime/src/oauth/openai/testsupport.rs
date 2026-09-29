//! Shared test doubles for the openai-oauth crate's unit tests.
//!
//! All seams the OAuth flow touches — HTTP transport, wall-clock, secure
//! storage, and the background spawner — are mocked here so the token
//! exchange, refresh, and login flows run fully offline (no network, no
//! browser, no real keychain).

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use lingxi_llm_client::{HttpRequest, StreamResponse, Transport};
use platform_api::{
    BackgroundTaskHandle, Clock, RuntimeError, RuntimeSpawner, SecureStorage, SecureStorageBackend,
    SecureStorageError,
};
use protocol::SecureStorageData;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
    body_routes: Vec<(String, Canned)>,
    pub requests: Mutex<Vec<HttpRequest>>,
    pub calls: AtomicU64,
}

impl MockHttp {
    /// Build a transport from `(url_substring, response)` routes.
    pub fn new(routes: Vec<(&str, Canned)>) -> Arc<Self> {
        Self::with_body_routes(routes, Vec::new())
    }

    /// Build a transport that can also discriminate on the request BODY.
    ///
    /// `body_routes` are matched first, by substring against `req.body`. The
    /// OpenAI token endpoint serves two different exchanges at one URL — the
    /// authorization-code grant and the RFC-8693 API-key mint — so a
    /// URL-keyed route cannot make one succeed and the other fail.
    pub fn with_body_routes(
        routes: Vec<(&str, Canned)>,
        body_routes: Vec<(&str, Canned)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            routes: routes
                .into_iter()
                .map(|(u, c)| (u.to_string(), c))
                .collect(),
            body_routes: body_routes
                .into_iter()
                .map(|(b, c)| (b.to_string(), c))
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
impl Transport for MockHttp {
    async fn send(
        &self,
        req: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(req.clone());
        let body_match = self
            .body_routes
            .iter()
            .find(|(needle, _)| String::from_utf8_lossy(&req.body).contains(needle.as_str()))
            .map(|(_, c)| c.clone());
        let canned = body_match
            .or_else(|| {
                self.routes
                    .iter()
                    .find(|(url, _)| req.url.contains(url.as_str()))
                    .map(|(_, c)| c.clone())
            })
            .or_else(|| self.routes.first().map(|(_, c)| c.clone()))
            .expect("no canned route");
        Ok(StreamResponse {
            status: canned.status,
            headers: vec![],
            body: Box::pin(stream::once(async move { Ok(Bytes::from(canned.body)) })),
        })
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

    /// The account names stored under `service`, sorted.
    ///
    /// [`Self::count`] is slot-blind: a write that lands in the wrong
    /// provider's slot keeps the total identical, which is how the ChatGPT
    /// refresh driver came to overwrite the Anthropic session while its tests
    /// stayed green. Assert against this instead whenever WHICH slot was
    /// written is the thing under test.
    pub fn accounts(&self, service: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .map
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, a)| a.clone())
            .collect();
        names.sort();
        names
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

/// Serialize tests that compete for a FIXED loopback port (OpenAI 1455/1457,
/// Anthropic 45321).
///
/// The guard is deliberately two-layered, because the resource is:
///
/// - a `static` mutex, so tests inside one binary queue rather than spin; and
/// - a LOCK FILE, because a TCP port is machine-global and `cargo test
///   --workspace` runs many test binaries at once. An in-process mutex is
///   simply the wrong scope for a machine-global resource — that mismatch is
///   what produced the intermittent `both fixed ports 1455 and 1457 are
///   already in use` failure, which reproduced only under a saturated
///   full-workspace run and passed every time in isolation.
///
/// The file lock is an `O_EXCL` create with a stale-takeover timeout rather
/// than `flock`, so it needs no new dependency and cannot wedge the suite if a
/// holder is killed mid-test.
pub async fn port_guard() -> PortGuard {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let in_process = LOCK.lock().await;
    let path = std::env::temp_dir().join("lingxi-oauth-fixed-port.lock");
    // Takeover is keyed ONLY on staleness — a lock file whose holder has been
    // gone for STALE_AFTER. There is deliberately no wall-clock deadline that
    // steals a LIVE lock: the first version of this had one (60s), and under a
    // saturated full-workspace run a legitimate holder exceeded it, so a waiter
    // stole the lock, both ran, both bound 1455, and the loser failed with
    // "both fixed ports are already in use" — the exact symptom the lock
    // exists to prevent. A safety valve that breaks the invariant it guards is
    // worse than a hang, because a hang is diagnosable.
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(300);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > STALE_AFTER)
                    // Un-stat-able: treat as stale, else an unreadable lock file
                    // wedges the suite forever.
                    .unwrap_or(true);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            // An unusable temp dir leaves only the in-process mutex. That still
            // covers same-binary contention, which is where these tests
            // actually collide.
            Err(_) => break,
        }
    }
    PortGuard {
        _in_process: in_process,
        path,
    }
}

/// Bind the fixed callback ports for a TEST, retrying briefly if both are
/// momentarily occupied.
///
/// Every in-binary binder already holds [`port_guard`], and `SO_REUSEADDR`
/// removed the `TIME_WAIT` rebind failure, yet
/// "both fixed ports 1455 and 1457 are already in use" still appeared
/// occasionally with nothing listening at rest. 1455/1457 are ordinary
/// registered ports on a shared developer machine: any other process can hold
/// them for a moment, and no amount of in-suite locking makes the machine
/// exclusive.
///
/// These tests are about the callback PROTOCOL — state validation, param
/// parsing, the success page — not about winning a port race. Retrying here
/// keeps the production `bind()` faithful (fixed ports, no retry, which IS the
/// parity behaviour) while stopping an unrelated machine condition from
/// failing an unrelated assertion.
///
/// # Panics
/// After `ATTEMPTS` failures, with the underlying bind error — a genuinely
/// wedged port still fails loudly rather than silently skipping.
pub async fn bind_fixed_ports_for_test() -> super::callback::CallbackListener {
    const ATTEMPTS: usize = 20;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        match super::callback::CallbackListener::bind().await {
            Ok(l) => return l,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    panic!("bind: {last:?} after {ATTEMPTS} attempts");
}

/// Held for the duration of a fixed-port test; releases both layers on drop.
pub struct PortGuard {
    _in_process: tokio::sync::MutexGuard<'static, ()>,
    path: std::path::PathBuf,
}

impl Drop for PortGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Build a `CredentialManager` over an in-memory store + fixed clock.
pub fn mem_credential_manager(
    storage: Arc<MemStorage>,
    clock: Arc<dyn Clock>,
) -> Arc<secret::CredentialManager> {
    struct CredentialHttp;
    #[async_trait]
    impl platform_api::HttpTransport for CredentialHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            panic!("CredentialManager network request was not expected in OAuth test")
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            panic!("CredentialManager stream request was not expected in OAuth test")
        }
    }
    Arc::new(secret::CredentialManager::new(
        storage as Arc<dyn SecureStorage>,
        clock,
        Arc::new(CredentialHttp) as Arc<dyn platform_api::HttpTransport>,
    ))
}
