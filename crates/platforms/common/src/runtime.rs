//! Tokio-backed [`RuntimeSpawner`] for hosts.
//!
//! Wraps `tokio::spawn` so engine code can request background tasks without
//! depending on tokio directly. The spawner keeps a registry of handles so
//! [`RuntimeSpawner::cancel`] can abort a running task by id.

use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Tokio-backed spawner. Holds the live `JoinHandle`s keyed by task id so
/// they can be aborted on demand.
pub struct TokioRuntime {
    next: AtomicU64,
    handles: Mutex<HashMap<u64, JoinHandle<()>>>,
}

impl TokioRuntime {
    /// Build a fresh `TokioRuntime` spawner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for TokioRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
#[allow(clippy::unwrap_used)] // std Mutex poison is treated as a fatal bug.
impl RuntimeSpawner for TokioRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.handles.lock().unwrap().insert(
            id,
            tokio::spawn(lingxi_core::host::model_safety::bind_current_model_safety(
                task,
            )),
        );
        Ok(BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: id,
        })
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        if let Some(h) = self.handles.lock().unwrap().remove(&handle.task_id) {
            h.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use lingxi_core::host::{model_safety, session_flags};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn spawned_tasks_keep_model_safety_and_request_attribution() {
        let runtime = TokioRuntime::new();
        let session = lingxi_core::types::SessionId::new();
        let observations = Arc::new(AtomicUsize::new(0));
        let recorded = observations.clone();
        let observer = model_safety::ModelSafetyObserver::new(move |_| {
            recorded.fetch_add(1, Ordering::SeqCst);
        });
        let (sent, received) = oneshot::channel();
        session_flags::scope_subagent_request_session_id(
            Some(session),
            model_safety::scope_model_safety(observer, async {
                runtime
                    .spawn(
                        "attribution",
                        Box::pin(async move {
                            let observer = model_safety::current_model_safety_observer();
                            if let Some(observer) = &observer {
                                observer.record(model_safety::ModelSafetyStop::Refusal);
                            }
                            let _ = sent.send((
                                session_flags::current_request_session_id(),
                                session_flags::current_request_is_subagent(),
                                observer.is_some(),
                            ));
                        }),
                    )
                    .await
                    .unwrap();
            }),
        )
        .await;
        let (observed_session, is_subagent, has_observer) = received.await.unwrap();
        assert_eq!(observed_session, Some(session));
        assert!(is_subagent);
        assert!(has_observer);
        assert_eq!(observations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_drops_a_running_task_and_is_idempotent() {
        struct SignalDrop(Option<oneshot::Sender<()>>);
        impl Drop for SignalDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let runtime = TokioRuntime::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let handle = runtime
            .spawn(
                "cancel",
                Box::pin(async move {
                    let _guard = SignalDrop(Some(dropped_tx));
                    let _ = started_tx.send(());
                    std::future::pending::<()>().await;
                }),
            )
            .await
            .unwrap();
        started_rx.await.unwrap();
        runtime.cancel(&handle).await.unwrap();
        runtime.cancel(&handle).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .expect("cancelled task must be dropped")
            .unwrap();
    }
}
