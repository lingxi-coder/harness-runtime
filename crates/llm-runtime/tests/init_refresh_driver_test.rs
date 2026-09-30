//! End-to-end smoke: `init_refresh_driver` registers the hook + spawns proactive.

use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, Clock, RuntimeError, RuntimeSpawner};
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::{HttpRequest, StreamResponse, Transport};
use llm_runtime::auth::anthropic::login::init_refresh_driver;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

struct NoopTransport;
#[async_trait]
impl Transport for NoopTransport {
    async fn send(
        &self,
        _req: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        Ok(StreamResponse {
            status: 200,
            headers: vec![],
            body: Box::pin(futures::stream::once(async {
                Ok(bytes::Bytes::from_static(b"{}"))
            })),
        })
    }
}
struct FixedClock(SystemTime);
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}
struct InertSpawner {
    next_id: AtomicU64,
    handles: Mutex<Vec<(u64, tokio::task::JoinHandle<()>)>>,
}
#[async_trait]
impl RuntimeSpawner for InertSpawner {
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
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
    async fn cancel(&self, h: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        let mut hs = self.handles.lock().await;
        if let Some(pos) = hs.iter().position(|(i, _)| *i == h.task_id) {
            let (_, jh) = hs.remove(pos);
            jh.abort();
            Ok(())
        } else {
            Err(RuntimeError::NotFound(h.task_name.clone()))
        }
    }
}

#[tokio::test]
async fn init_refresh_driver_returns_state_and_spawns_proactive() {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let transport: Arc<dyn Transport> = Arc::new(NoopTransport);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(now));
    let spawner: Arc<dyn RuntimeSpawner> = Arc::new(InertSpawner {
        next_id: AtomicU64::new(0),
        handles: Mutex::new(vec![]),
    });

    let state = init_refresh_driver(
        cfg,
        Secret::new("ACCESS".into()),
        Some(Secret::new("REFRESH".into())),
        now + Duration::from_secs(3600),
        transport,
        clock,
        None,
        None,
        spawner.clone(),
    )
    .await
    .expect("init ok");

    // Proactive handle was stored.
    assert!(state.proactive_handle().await.is_some());

    // Clean up: shutdown for a tidy test environment.
    state.shutdown(&*spawner).await;
}
