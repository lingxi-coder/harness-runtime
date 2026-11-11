//! Owned blocking file work. A cancelled waiter cannot leave a write behind.

use once_cell::sync::Lazy;
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tool_api::tool_trait::ToolError;

static JOBS: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(4)));
// Native synchronous writes are serialized. Retain that commit ordering while
// permitting reads/preparation on four workers, so validation + truncation
// cannot interleave between two tool-owned transactions.
static COMMITS: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(1)));

pub(crate) fn check_cancel(cancel: Option<&CancellationToken>) -> Result<(), ToolError> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        Err(ToolError::Aborted)
    } else {
        Ok(())
    }
}

async fn permit(
    cancel: &CancellationToken,
) -> Result<tokio::sync::OwnedSemaphorePermit, ToolError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ToolError::Aborted),
        permit = JOBS.clone().acquire_owned() => permit.map_err(|_| ToolError::Aborted),
    }
}

/// Read-only preparation may finish off-thread after its waiter is dropped.
/// It never mutates files or publishes tool state.
pub(crate) async fn prepare<T: Send + 'static>(
    cancel: Option<&CancellationToken>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ToolError> {
    let cancel = cancel.cloned().unwrap_or_default();
    let permit = permit(&cancel).await?;
    let worker_cancel = cancel.clone();
    let lease = lingxi_core::host::memory_tool_scope::current_memory_settlement()
        .map(|owner| owner.enter());
    let task = tokio::task::spawn_blocking(move || {
        let (_permit, _lease) = (permit, lease);
        check_cancel(Some(&worker_cancel))?;
        Ok(work())
    });
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ToolError::Aborted),
        result = task => result.map_err(join_error)?,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Running,
    Cancelled,
    Finished,
}

struct Settlement {
    phase: Mutex<Phase>,
    changed: Condvar,
}

struct Owner(Arc<Settlement>);

impl Drop for Owner {
    fn drop(&mut self) {
        let mut phase = self.0.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Queued {
            *phase = Phase::Cancelled;
        }
        // No async work is allowed in the commit closure. Waiting here is only
        // necessary when cancellation races an already-started OS write. The
        // write and its read-state publication settle before cancellation can
        // return, including on a current-thread runtime or inside a LocalSet.
        while *phase == Phase::Running {
            phase = self
                .0
                .changed
                .wait(phase)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct Finished(Arc<Settlement>);

impl Drop for Finished {
    fn drop(&mut self) {
        *self.0.phase.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Finished;
        self.0.changed.notify_all();
    }
}

/// Run a synchronous write transaction, including its read-state publication.
/// Before commit, cancellation suppresses the write. Once commit starts, its
/// owner joins that finite synchronous section even when its future is dropped.
pub(crate) async fn commit<T: Send + 'static>(
    cancel: Option<&CancellationToken>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ToolError> {
    commit_with_semaphore(cancel, work, COMMITS.clone()).await
}

async fn commit_with_semaphore<T: Send + 'static>(
    cancel: Option<&CancellationToken>,
    work: impl FnOnce() -> T + Send + 'static,
    commits: Arc<Semaphore>,
) -> Result<T, ToolError> {
    let cancel = cancel.cloned().unwrap_or_default();
    let permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(ToolError::Aborted),
        permit = commits.acquire_owned() => permit.map_err(|_| ToolError::Aborted)?,
    };
    let state = Arc::new(Settlement {
        phase: Mutex::new(Phase::Queued),
        changed: Condvar::new(),
    });
    let owner = Owner(state.clone());
    let worker_cancel = cancel.clone();
    let lease = lingxi_core::host::memory_tool_scope::current_memory_settlement()
        .map(|owner| owner.enter());
    let task = tokio::task::spawn_blocking(move || {
        let (_permit, _lease) = (permit, lease);
        {
            let mut phase = state.phase.lock().unwrap_or_else(|e| e.into_inner());
            if *phase == Phase::Cancelled || worker_cancel.is_cancelled() {
                *phase = Phase::Finished;
                return Err(ToolError::Aborted);
            }
            *phase = Phase::Running;
        }
        let _finished = Finished(state);
        Ok(work())
    });
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ToolError::Aborted),
        result = task => result.map_err(join_error)?,
    };
    drop(owner);
    result
}

fn join_error(error: tokio::task::JoinError) -> ToolError {
    ToolError::Io(format!("File operation task failed: {error}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test(flavor = "current_thread")]
    async fn localset_commit_keeps_runtime_responsive() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let heartbeat = Arc::new(AtomicBool::new(false));
                let observed = heartbeat.clone();
                let tick = tokio::task::spawn_local(async move {
                    tokio::task::yield_now().await;
                    heartbeat.store(true, Ordering::Release);
                });
                commit(None, move || {
                    while !observed.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                })
                .await
                .unwrap();
                tick.await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn cancelled_commit_does_not_run() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let changed = Arc::new(AtomicBool::new(false));
        let worker_changed = changed.clone();
        assert!(matches!(
            commit(Some(&cancel), move || worker_changed
                .store(true, Ordering::Release))
            .await,
            Err(ToolError::Aborted)
        ));
        assert!(!changed.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_started_commit_waits_for_state_publication() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let published = Arc::new(AtomicBool::new(false));
        let worker_published = published.clone();
        let mut task = Box::pin(commit(None, move || {
            let _ = started_tx.send(());
            release_rx.recv().unwrap();
            worker_published.store(true, Ordering::Release);
        }));
        tokio::select! {
            result = &mut task => panic!("commit finished before release: {result:?}"),
            _ = started_rx => {},
        }
        let releaser = std::thread::spawn(move || release_tx.send(()).unwrap());
        drop(task);
        assert!(published.load(Ordering::Acquire));
        releaser.join().unwrap();
    }

    #[tokio::test]
    async fn worker_panic_releases_owner() {
        assert!(commit(None, || panic!("owned test panic")).await.is_err());
        assert_eq!(commit(None, || 7).await.unwrap(), 7);
    }

    #[test]
    fn dropping_queued_commit_suppresses_later_write() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let worker_changed = changed.clone();
        runtime.block_on(async {
            let mut queued = Box::pin(commit_with_semaphore(
                None,
                move || {
                    worker_changed.store(true, Ordering::Release);
                },
                Arc::new(Semaphore::new(1)),
            ));
            poll_fn(|cx| {
                assert!(queued.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(queued);
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
        });
        // Dropping the runtime joins its queued blocking jobs.
        drop(runtime);
        assert!(!changed.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_cancellation_settles_started_commit_before_returning() {
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let published = Arc::new(AtomicBool::new(false));
        let worker_published = published.clone();
        let result = commit_with_semaphore(
            Some(&cancel),
            move || {
                worker_cancel.cancel();
                worker_published.store(true, Ordering::Release);
            },
            Arc::new(Semaphore::new(1)),
        )
        .await;
        assert!(matches!(result, Err(ToolError::Aborted)));
        assert!(published.load(Ordering::Acquire));
    }
}
