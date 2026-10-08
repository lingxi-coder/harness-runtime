//! Trusted session attribution for model safety observations. The capability
//! never enters model input, persisted fees, or provider protocol data.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSafetyStop {
    Refusal,
    ContentFiltering,
}

#[derive(Clone)]
pub struct ModelSafetyObserver(Arc<dyn Fn(ModelSafetyStop) + Send + Sync>);

impl ModelSafetyObserver {
    pub fn new(record: impl Fn(ModelSafetyStop) + Send + Sync + 'static) -> Self {
        Self(Arc::new(record))
    }

    /// A non-panicking observational callback cannot affect model execution.
    pub fn record(&self, event: ModelSafetyStop) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.0)(event))).is_err() {
            tracing::warn!("model safety observation callback panicked");
        }
    }
}

impl std::fmt::Debug for ModelSafetyObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ModelSafetyObserver(<session authority>)")
    }
}

impl PartialEq for ModelSafetyObserver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

tokio::task_local! {
    static MODEL_SAFETY_OBSERVER: ModelSafetyObserver;
}

pub async fn scope_model_safety<T>(
    observer: ModelSafetyObserver,
    future: impl std::future::Future<Output = T>,
) -> T {
    MODEL_SAFETY_OBSERVER.scope(observer, future).await
}

pub fn current_model_safety_observer() -> Option<ModelSafetyObserver> {
    MODEL_SAFETY_OBSERVER.try_with(Clone::clone).ok()
}

/// Capture attribution before transferring an existing future to another task.
pub fn bind_current_model_safety<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    let future = crate::host::session_flags::bind_current_request_session_id(future);
    bind_model_safety(current_model_safety_observer(), future)
}

/// Bind an already captured origin to existing work before task transfer.
pub fn bind_model_safety<F: std::future::Future>(
    observer: Option<ModelSafetyObserver>,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    async move {
        if let Some(observer) = observer {
            scope_model_safety(observer, future).await
        } else {
            future.await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[tokio::test]
    async fn transferring_work_captures_origin_before_the_active_scope_changes() {
        let old = Arc::new(AtomicU64::new(0));
        let new = Arc::new(AtomicU64::new(0));
        let observer = |count: Arc<AtomicU64>| {
            ModelSafetyObserver::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            })
        };
        let work = scope_model_safety(observer(old.clone()), async {
            bind_current_model_safety(async {
                current_model_safety_observer()
                    .unwrap()
                    .record(ModelSafetyStop::Refusal);
            })
        })
        .await;
        scope_model_safety(observer(new.clone()), async {
            tokio::spawn(work).await.unwrap();
        })
        .await;
        assert_eq!(old.load(Ordering::Relaxed), 1);
        assert_eq!(new.load(Ordering::Relaxed), 0);
        assert!(current_model_safety_observer().is_none());
    }
}
