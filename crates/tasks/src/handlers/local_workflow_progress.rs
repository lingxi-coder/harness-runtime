//! Bounded workflow progress shared by synchronous script callbacks and async
//! agent observers. An oversized event is kept intact and owns the whole byte
//! budget until consumed, so backlog is bounded by the budget plus one event.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

const CANCEL_POLL: Duration = Duration::from_millis(5);

#[derive(Clone)]
pub(super) struct ProgressBudget {
    semaphore: Arc<Semaphore>,
    bytes: u32,
    order: Arc<tokio::sync::Mutex<()>>,
}

impl ProgressBudget {
    pub(super) fn new(bytes: u32) -> Self {
        assert!(bytes > 0);
        Self {
            semaphore: Arc::new(Semaphore::new(bytes as usize)),
            bytes,
            order: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// Keep the byte permit until the consumer has persisted/delivered the value,
/// rather than releasing it as soon as a queue slot becomes available.
pub(super) struct ProgressItem<T> {
    pub(super) value: T,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(super) enum ProgressSender<T> {
    Bounded {
        tx: mpsc::Sender<ProgressItem<T>>,
        budget: ProgressBudget,
        cancel: Arc<AtomicBool>,
        size: fn(&T) -> usize,
    },
    // Existing bridge tests can observe collecting fixtures without spawning
    // a drainer. Shipping task runs always use the bounded variant.
    #[cfg(test)]
    Unbounded(mpsc::UnboundedSender<T>),
}

pub(super) fn channel<T>(
    capacity: usize,
    budget: ProgressBudget,
    cancel: Arc<AtomicBool>,
    size: fn(&T) -> usize,
) -> (ProgressSender<T>, mpsc::Receiver<ProgressItem<T>>) {
    let (tx, rx) = mpsc::channel(capacity);
    (
        ProgressSender::Bounded {
            tx,
            budget,
            cancel,
            size,
        },
        rx,
    )
}

#[cfg(test)]
impl<T> From<mpsc::UnboundedSender<T>> for ProgressSender<T> {
    fn from(tx: mpsc::UnboundedSender<T>) -> Self {
        Self::Unbounded(tx)
    }
}

impl<T> ProgressSender<T> {
    /// Paired spool/live publication uses one gate so awaiting backpressure
    /// cannot put an observer's live update ahead of an earlier spool event.
    pub(super) fn order(&self) -> Option<Arc<tokio::sync::Mutex<()>>> {
        match self {
            Self::Bounded { budget, .. } => Some(budget.order.clone()),
            #[cfg(test)]
            Self::Unbounded(_) => None,
        }
    }

    pub(super) async fn send(&self, value: T) -> bool {
        let (tx, budget, cancel, size) = match self {
            Self::Bounded {
                tx,
                budget,
                cancel,
                size,
            } => (tx, budget, cancel, size),
            #[cfg(test)]
            Self::Unbounded(tx) => return tx.send(value).is_ok(),
        };
        let bytes = size(&value).clamp(1, budget.bytes as usize) as u32;
        let send = async {
            let Ok(permit) = budget.semaphore.clone().acquire_many_owned(bytes).await else {
                return false;
            };
            tx.send(ProgressItem {
                value,
                _permit: permit,
            })
            .await
            .is_ok()
        };
        tokio::pin!(send);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            tokio::select! {
                result = &mut send => return result,
                () = tx.closed() => return false,
                () = tokio::time::sleep(CANCEL_POLL) => {}
            }
        }
    }

    /// Only used on the dedicated script OS thread. `try_send` + a short park
    /// works even when that thread has entered a Tokio current-thread runtime;
    /// Tokio's `blocking_send`/`blocking_lock` would panic in that context.
    pub(super) fn send_sync(&self, value: T) -> bool {
        let (tx, budget, cancel, size) = match self {
            Self::Bounded {
                tx,
                budget,
                cancel,
                size,
            } => (tx, budget, cancel, size),
            #[cfg(test)]
            Self::Unbounded(tx) => return tx.send(value).is_ok(),
        };
        let bytes = size(&value).clamp(1, budget.bytes as usize) as u32;
        let permit = loop {
            if cancel.load(Ordering::Relaxed) || tx.is_closed() {
                return false;
            }
            if let Ok(permit) = budget.semaphore.clone().try_acquire_many_owned(bytes) {
                break permit;
            }
            std::thread::park_timeout(CANCEL_POLL);
        };
        let mut item = ProgressItem {
            value,
            _permit: permit,
        };
        loop {
            if cancel.load(Ordering::Relaxed) || tx.is_closed() {
                return false;
            }
            match tx.try_send(item) {
                Ok(()) => return true,
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
                Err(mpsc::error::TrySendError::Full(returned)) => item = returned,
            }
            std::thread::park_timeout(CANCEL_POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_progress_holds_the_shared_budget_until_delivery() {
        let budget = ProgressBudget::new(4);
        let cancel = Arc::new(AtomicBool::new(false));
        let (spool, mut spool_rx) = channel(2, budget.clone(), cancel.clone(), String::len);
        let (live, mut live_rx) = channel(2, budget.clone(), cancel, String::len);
        let large = "large log".repeat(1024);
        assert!(spool.send(large.clone()).await);
        let item = spool_rx.recv().await.expect("large spool item");
        assert_eq!(item.value, large);
        assert_eq!(budget.semaphore.available_permits(), 0);

        let send_live = live.send("next".to_string());
        tokio::pin!(send_live);
        assert!(futures::poll!(&mut send_live).is_pending());
        assert!(live_rx.try_recv().is_err());
        // Dequeueing does not release the budget while persistence/delivery
        // still owns the oversized item.
        assert_eq!(budget.semaphore.available_permits(), 0);
        drop(item);
        assert!(send_live.await);
        let delivered = live_rx.recv().await.expect("live item after spool write");
        assert_eq!(delivered.value, "next");
        drop(delivered);
        assert_eq!(budget.semaphore.available_permits(), 4);
    }

    #[tokio::test]
    async fn cancel_unblocks_async_progress_waiting_for_capacity() {
        let budget = ProgressBudget::new(4);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = channel(1, budget.clone(), cancel.clone(), String::len);
        assert!(tx.send("full".to_string()).await);
        let waiting = tx.send("blocked".to_string());
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        cancel.store(true, Ordering::Relaxed);
        assert!(!tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancel must release an async producer"));
        drop(rx.recv().await);
        assert_eq!(budget.semaphore.available_permits(), 4);
    }

    #[tokio::test]
    async fn closed_consumer_unblocks_progress_waiting_for_a_byte_permit() {
        let budget = ProgressBudget::new(4);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = channel(1, budget, cancel, String::len);
        assert!(tx.send("full".to_string()).await);
        let retained = rx.recv().await.expect("consumer owns the permit");
        let waiting = tx.send("blocked".to_string());
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(rx);
        assert!(!tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("closed consumer must release the producer"));
        drop(retained);
    }

    #[tokio::test]
    async fn script_backpressure_works_inside_a_current_thread_runtime() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = channel(1, ProgressBudget::new(8), cancel, String::len);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("script runtime");
            runtime.block_on(async move {
                assert!(tx.send_sync("one".to_string()));
                let _ = started_tx.send(());
                let _ = done_tx.send(tx.send_sync("two".to_string()));
            });
        });
        started_rx.await.expect("script sent its first event");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut done_rx)
                .await
                .is_err()
        );
        let first = rx.recv().await.expect("first event");
        assert_eq!(first.value, "one");
        drop(first);
        assert!(tokio::time::timeout(Duration::from_secs(1), done_rx)
            .await
            .expect("script resumes after capacity is available")
            .expect("script result"));
        let second = rx.recv().await.expect("second event");
        assert_eq!(second.value, "two");
        thread
            .join()
            .expect("script must not panic in Tokio context");
    }

    #[tokio::test]
    async fn cancel_unblocks_script_progress_waiting_for_a_byte_permit() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = channel(1, ProgressBudget::new(4), cancel.clone(), String::len);
        assert!(tx.send("full".to_string()).await);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("script runtime");
            runtime.block_on(async move {
                let _ = started_tx.send(());
                let _ = done_tx.send(tx.send_sync("blocked large log".repeat(1024)));
            });
        });
        started_rx.await.expect("script producer started");
        let mut done_rx = done_rx;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut done_rx)
                .await
                .is_err()
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(!tokio::time::timeout(Duration::from_secs(1), done_rx)
            .await
            .expect("cancel releases the script producer")
            .expect("script result"));
        drop(rx.recv().await);
        thread.join().expect("script must exit without a panic");
    }
}
