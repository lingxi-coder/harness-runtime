//! Proactive refresh: short-lived token (TTL < 5 min) triggers refresh at
//! remaining/2 (not at a fixed 5 min lead). Handle is owned by `AuthState` and
//! cancellable via `AuthState::shutdown` (Task 6).

use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, Clock, RuntimeError, RuntimeSpawner};
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::{HttpRequest, StreamResponse, Transport};
use llm_runtime::auth::anthropic::refresh::{AuthState, RefreshDriver};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use telemetry::sink::{AnalyticsSink, LogEventMetadata};
use tokio::sync::Mutex;

struct CountingTransport {
    calls: Arc<AtomicU32>,
    expires_in_secs: u64,
}

#[async_trait]
impl Transport for CountingTransport {
    async fn send(
        &self,
        _req: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let body = format!(
            r#"{{"access_token":"REFRESHED_v{n}","refresh_token":"R_v{n}","expires_in":{}}}"#,
            self.expires_in_secs,
        );
        Ok(StreamResponse {
            status: 200,
            headers: vec![],
            body: Box::pin(futures::stream::once(async move {
                Ok(bytes::Bytes::from(body))
            })),
        })
    }
}

struct AdvancingClock {
    base: SystemTime,
    elapsed: Arc<AtomicU64>,
}
impl Clock for AdvancingClock {
    fn now(&self) -> SystemTime {
        self.base + Duration::from_millis(self.elapsed.load(Ordering::SeqCst))
    }
}

/// Spawner that runs tasks on the tokio runtime and tracks handles via tokio
/// `JoinHandle` stored in a Mutex. Cancellation aborts the `JoinHandle`.
struct TokioSpawner {
    next_id: AtomicU64,
    handles: Mutex<Vec<(u64, tokio::task::JoinHandle<()>)>>,
}

#[async_trait]
impl RuntimeSpawner for TokioSpawner {
    async fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let jh = tokio::spawn(task);
        self.handles.lock().await.push((id, jh));
        Ok(BackgroundTaskHandle {
            task_name: name.into(),
            task_id: id,
        })
    }
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
    async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        let mut h = self.handles.lock().await;
        if let Some(pos) = h.iter().position(|(id, _)| *id == handle.task_id) {
            let (_, jh) = h.remove(pos);
            jh.abort();
            Ok(())
        } else {
            Err(RuntimeError::NotFound(handle.task_name.clone()))
        }
    }
}

#[tokio::test(start_paused = true)]
async fn short_ttl_token_refreshes_at_half_remaining() {
    // Construct a token with 60-second TTL. proactive_lead = 30s (remaining/2).
    // The task should wake at remaining(60) - lead(30) = 30s after construction.
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let calls = Arc::new(AtomicU32::new(0));
    let elapsed = Arc::new(AtomicU64::new(0));
    let transport: Arc<dyn Transport> = Arc::new(CountingTransport {
        calls: calls.clone(),
        expires_in_secs: 60,
    });
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let clock: Arc<dyn Clock> = Arc::new(AdvancingClock {
        base,
        elapsed: elapsed.clone(),
    });
    let spawner: Arc<dyn RuntimeSpawner> = Arc::new(TokioSpawner {
        next_id: AtomicU64::new(0),
        handles: Mutex::new(vec![]),
    });

    let state = AuthState::new(
        cfg,
        Secret::new("INITIAL".to_string()),
        Some(Secret::new("INITIAL_R".to_string())),
        base + Duration::from_secs(60),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes,
        transport,
        clock,
        None,
        None,
    );

    RefreshDriver::spawn_proactive(state.clone(), spawner.clone())
        .await
        .expect("spawn ok");
    assert!(state.proactive_handle().await.is_some(), "handle stored");

    // Yield once so the spawned task gets scheduled and reaches its first
    // `await` point (the sleep). Without this the `advance` call below may
    // fire before the task has parked on the timer.
    tokio::task::yield_now().await;

    // Advance virtual time past the wake instant (30s). Real clock used by the
    // background sleep_until is tokio's auto-advance under `start_paused=true`.
    tokio::time::advance(Duration::from_secs(31)).await;
    // Yield to let the task observe the wake.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;

    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "expected proactive refresh fired at remaining/2 = 30s, calls = {}",
        calls.load(Ordering::SeqCst),
    );
}

#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<(String, LogEventMetadata)>>,
}
#[async_trait]
impl AnalyticsSink for CaptureSink {
    async fn log_event(&self, name: &str, m: LogEventMetadata) {
        self.events.lock().await.push((name.into(), m));
    }
    async fn log_event_async(&self, name: &str, m: LogEventMetadata) {
        self.events.lock().await.push((name.into(), m));
    }
    fn name(&self) -> &str {
        "capture"
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_handle_and_emits_event() {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let calls = Arc::new(AtomicU32::new(0));
    let elapsed = Arc::new(AtomicU64::new(0));
    let transport: Arc<dyn Transport> = Arc::new(CountingTransport {
        calls: calls.clone(),
        expires_in_secs: 3600,
    });
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let clock: Arc<dyn Clock> = Arc::new(AdvancingClock { base, elapsed });
    let spawner: Arc<dyn RuntimeSpawner> = Arc::new(TokioSpawner {
        next_id: AtomicU64::new(0),
        handles: Mutex::new(vec![]),
    });
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let sink = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone()).await;

    let state = AuthState::new(
        cfg,
        Secret::new("INITIAL".into()),
        Some(Secret::new("INITIAL_R".into())),
        base + Duration::from_secs(3600),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes,
        transport,
        clock,
        Some(bus.clone()),
        None,
    );

    RefreshDriver::spawn_proactive(state.clone(), spawner.clone())
        .await
        .expect("spawn ok");
    assert!(state.proactive_handle().await.is_some());

    // First shutdown — cancels the handle and emits the event.
    state.shutdown(&*spawner).await;
    assert!(state.proactive_handle().await.is_none(), "handle cleared");

    let events = sink.events.lock().await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"tengu_oauth_proactive_canceled"),
        "events: {names:?}",
    );
    drop(events);

    // Second shutdown — idempotent, no panic.
    state.shutdown(&*spawner).await;
    assert!(state.proactive_handle().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn proactive_then_reactive_collapses_to_one_refresh() {
    // Same setup as `short_ttl_token_refreshes_at_half_remaining`, but we
    // additionally fire a reactive `refresh()` while the proactive task is
    // sleeping. Both must collapse to a single HTTP call thanks to the shared
    // refresh_lock + double-check-after-acquire.
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let calls = Arc::new(AtomicU32::new(0));
    let elapsed = Arc::new(AtomicU64::new(0));
    let transport: Arc<dyn Transport> = Arc::new(CountingTransport {
        calls: calls.clone(),
        expires_in_secs: 3600,
    });
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let clock: Arc<dyn Clock> = Arc::new(AdvancingClock { base, elapsed });
    let spawner: Arc<dyn RuntimeSpawner> = Arc::new(TokioSpawner {
        next_id: AtomicU64::new(0),
        handles: Mutex::new(vec![]),
    });

    let state = AuthState::new(
        cfg,
        Secret::new("INITIAL".into()),
        Some(Secret::new("R_INITIAL".into())),
        base + Duration::from_secs(60),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes, // 1-minute TTL so proactive wakes at 30s
        transport,
        clock,
        None,
        None,
    );

    RefreshDriver::spawn_proactive(state.clone(), spawner.clone())
        .await
        .expect("spawn ok");

    // Fire a reactive refresh BEFORE the proactive timer's 30s wake.
    let driver = llm_runtime::auth::anthropic::refresh::RefreshDriver::new(state.clone());
    let prev = state.token.read().await.token_hash();
    let r = driver.refresh(prev).await;
    assert!(r.is_ok(), "reactive refresh ok: {r:?}");
    let after_reactive = calls.load(Ordering::SeqCst);
    assert_eq!(after_reactive, 1, "reactive made exactly one call");

    // Now advance time past the proactive wake. The proactive task will read
    // the new (already-rotated) expiry and reschedule against it; it should
    // NOT make a second HTTP call yet because the new expiry is 1 hour away.
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "proactive saw the freshly-rotated token (expiry now 1h away); no second HTTP call",
    );

    state.shutdown(&*spawner).await;
}
