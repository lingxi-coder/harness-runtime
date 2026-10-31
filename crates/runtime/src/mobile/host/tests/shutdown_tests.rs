use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lingxi_core::host::bg_session_forker::lifecycle::{
    BackgroundLaunchLifecycle, BackgroundLaunchRecovery,
};
use tokio::sync::Notify;

struct SignalTaskDrop(std::sync::mpsc::Sender<()>);

impl Drop for SignalTaskDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

fn build_test_handle(root: &std::path::Path) -> Arc<super::MobileEngineHandle> {
    use crate::mobile::test_support::{
        test_config, CollectingPermissionSink, FakeListener, HostFakePlatform,
    };
    super::build_mobile_engine(
        test_config(root),
        Arc::new(HostFakePlatform::new(root.into())),
        Arc::new(FakeListener::default()),
        Arc::new(CollectingPermissionSink::default()),
    )
    .unwrap()
}

struct RecoverAfterRelease {
    ready: AtomicBool,
    attempts: AtomicUsize,
    changed: Notify,
}

#[async_trait::async_trait]
impl BackgroundLaunchRecovery for RecoverAfterRelease {
    fn description(&self) -> String {
        "mobile test recovery".into()
    }

    async fn recover(&self) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        if self.ready.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("recovery is not yet confirmed".into())
        }
    }
}

async fn wait_for_count(count: &AtomicUsize, changed: &Notify, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if count.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("shutdown stage did not advance");
}

#[tokio::test]
async fn mobile_shutdown_retains_dependencies_until_launch_and_prewarm_are_confirmed() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let recovery = Arc::new(RecoverAfterRelease {
        ready: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
        changed: Notify::new(),
    });
    let claim = lifecycle.claim().unwrap();
    drop(claim.register_recovery(recovery.clone()));

    // These stand in for the engine executor, task/tool registries, and writer
    // retained by the real Drop task.
    let executor = Arc::new(());
    let registry = Arc::new(());
    let writer = Arc::new(());
    let weak = (
        Arc::downgrade(&executor),
        Arc::downgrade(&registry),
        Arc::downgrade(&writer),
    );
    let launch_attempts = Arc::new(AtomicUsize::new(0));
    let launch_changed = Arc::new(Notify::new());
    let prewarm_release = Arc::new(Notify::new());
    let prewarm_done = Arc::new(AtomicBool::new(false));
    let shutdown = tokio::spawn({
        let lifecycle = lifecycle.clone();
        let launch_attempts = launch_attempts.clone();
        let launch_changed = launch_changed.clone();
        let prewarm_release = prewarm_release.clone();
        let prewarm_done = prewarm_done.clone();
        async move {
            let retained = super::wait_for_mobile_background_producers(
                (executor, registry, writer),
                move || {
                    let lifecycle = lifecycle.clone();
                    launch_attempts.fetch_add(1, Ordering::SeqCst);
                    launch_changed.notify_waiters();
                    async move { lifecycle.close_and_drain().await }
                },
                move || {
                    let prewarm_release = prewarm_release.clone();
                    let prewarm_done = prewarm_done.clone();
                    async move {
                        loop {
                            let notified = prewarm_release.notified();
                            tokio::pin!(notified);
                            notified.as_mut().enable();
                            if prewarm_done.load(Ordering::SeqCst) {
                                break Vec::new();
                            }
                            notified.await;
                        }
                    }
                },
                Duration::from_millis(20),
                Duration::from_millis(20),
                Duration::from_millis(40),
            )
            .await;
            drop(retained);
        }
    });

    // An active launch and prewarm outlive the first bounded drain attempt.
    wait_for_count(&launch_attempts, &launch_changed, 2).await;
    assert!(lifecycle.claim().is_err());
    assert!(!shutdown.is_finished());
    assert!(weak.0.upgrade().is_some());
    assert!(weak.1.upgrade().is_some());
    assert!(weak.2.upgrade().is_some());

    prewarm_done.store(true, Ordering::SeqCst);
    prewarm_release.notify_waiters();
    drop(claim);
    wait_for_count(&recovery.attempts, &recovery.changed, 1).await;
    assert!(!shutdown.is_finished());
    assert!(weak.0.upgrade().is_some());
    assert!(weak.1.upgrade().is_some());
    assert!(weak.2.upgrade().is_some());

    recovery.ready.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown did not retry recovery")
        .expect("shutdown task failed");
    assert!(recovery.attempts.load(Ordering::SeqCst) >= 2);
    assert!(weak.0.upgrade().is_none());
    assert!(weak.1.upgrade().is_none());
    assert!(weak.2.upgrade().is_none());
}

#[tokio::test]
async fn shutdown_rechecks_the_turn_slot_after_the_watcher_exits() {
    let slot = Arc::new(tokio::sync::Mutex::new(None));
    let turn = Arc::new(super::ActiveTurn::new(None));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let watcher = tokio::spawn({
        let slot = slot.clone();
        let turn = turn.clone();
        async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            *slot.lock().await = Some(turn);
        }
    });
    started_rx.await.unwrap();
    let cancellation = tokio::spawn(super::cancel_mobile_turn_after_watcher(Some(watcher), slot));
    tokio::task::yield_now().await;
    assert!(!cancellation.is_finished());
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), turn.cancel.cancelled())
        .await
        .expect("the late turn must be cancelled");
    turn.mark_completed();
    cancellation.await.unwrap();
}

#[test]
fn mobile_drop_keeps_runtime_and_authorities_while_prewarm_releases() {
    use std::sync::mpsc;

    struct BlockPrewarmDrop {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Drop for BlockPrewarmDrop {
        fn drop(&mut self) {
            let _ = self.started.send(());
            let _ = self.release.recv_timeout(Duration::from_secs(10));
        }
    }
    let root = tempfile::tempdir().unwrap();
    let handle = build_test_handle(root.path());
    let registry = Arc::downgrade(&handle.inner.task_registry);
    let writer = Arc::downgrade(&handle.inner.session_writer);
    let (work_started_tx, work_started_rx) = mpsc::channel();
    let (drop_started_tx, drop_started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (task_started_tx, task_started_rx) = mpsc::channel();
    let (task_dropped_tx, task_dropped_rx) = mpsc::channel();

    handle.runtime().block_on(async {
        handle
            .inner
            .orchestrator
            .spawn_startup_prewarm_for_test(async move {
                let _guard = BlockPrewarmDrop {
                    started: drop_started_tx,
                    release: release_rx,
                };
                work_started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            });
        drop(tokio::spawn(async move {
            let _guard = SignalTaskDrop(task_dropped_tx);
            task_started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
    });
    work_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    task_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();

    drop(handle);
    drop_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(registry.upgrade().is_some());
    assert!(writer.upgrade().is_some());
    assert!(matches!(
        task_dropped_rx.recv_timeout(Duration::from_millis(2250)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    release_tx.send(()).unwrap();
    task_dropped_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the owned runtime should shut down after prewarm releases");
}

#[test]
fn mobile_drop_waits_for_tracked_turn_after_its_slot_is_cleared() {
    use std::sync::mpsc;

    let root = tempfile::tempdir().unwrap();
    let handle = build_test_handle(root.path());
    let (slot_cleared_tx, slot_cleared_rx) = mpsc::channel();
    let (turn_release_tx, turn_release_rx) = tokio::sync::oneshot::channel();
    let (sentinel_started_tx, sentinel_started_rx) = mpsc::channel();
    let (sentinel_dropped_tx, sentinel_dropped_rx) = mpsc::channel();
    let slot = handle.active_cancel.clone();
    handle.runtime().block_on(async {
        let turn = Arc::new(super::ActiveTurn::new(None));
        *slot.lock().await = Some(turn.clone());
        let task = handle
            .runtime()
            .spawn(handle.turn_tasks.track_future(async move {
                *slot.lock().await = None;
                slot_cleared_tx.send(()).unwrap();
                let _ = turn_release_rx.await;
            }));
        turn.set_task_handle(task);
        drop(tokio::spawn(async move {
            let _guard = SignalTaskDrop(sentinel_dropped_tx);
            sentinel_started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
    });
    slot_cleared_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    sentinel_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();

    drop(handle);
    assert!(matches!(
        sentinel_dropped_rx.recv_timeout(Duration::from_millis(250)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    turn_release_tx.send(()).unwrap();
    sentinel_dropped_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("runtime should shut down after the turn future is released");
}

#[test]
fn mobile_drop_does_not_wait_forever_for_an_unspawned_turn_reservation() {
    use std::sync::mpsc;

    let root = tempfile::tempdir().unwrap();
    let handle = build_test_handle(root.path());
    let (sentinel_started_tx, sentinel_started_rx) = mpsc::channel();
    let (sentinel_dropped_tx, sentinel_dropped_rx) = mpsc::channel();
    handle.runtime().block_on(async {
        *handle.active_cancel.lock().await = Some(Arc::new(super::ActiveTurn::new(None)));
        drop(tokio::spawn(async move {
            let _guard = SignalTaskDrop(sentinel_dropped_tx);
            sentinel_started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
    });
    sentinel_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    drop(handle);
    sentinel_dropped_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("an abandoned reservation must not hold the runtime open");
}

#[test]
fn mobile_drop_can_abort_a_turn_after_its_join_handle_was_taken() {
    use std::sync::mpsc;

    let root = tempfile::tempdir().unwrap();
    let handle = build_test_handle(root.path());
    let (turn_started_tx, turn_started_rx) = mpsc::channel();
    let (sentinel_started_tx, sentinel_started_rx) = mpsc::channel();
    let (sentinel_dropped_tx, sentinel_dropped_rx) = mpsc::channel();
    handle.runtime().block_on(async {
        let turn = Arc::new(super::ActiveTurn::new(None));
        *handle.active_cancel.lock().await = Some(turn.clone());
        let task = handle
            .runtime()
            .spawn(handle.turn_tasks.track_future(async move {
                turn_started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            }));
        turn.set_task_handle(task);
        // A cancelled Pause/Cancel waiter may have taken and lost the join.
        drop(turn.take_task_handle().unwrap());
        drop(tokio::spawn(async move {
            let _guard = SignalTaskDrop(sentinel_dropped_tx);
            sentinel_started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
    });
    turn_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    sentinel_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    drop(handle);
    sentinel_dropped_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the timeout must abort the turn despite the missing join handle");
}
