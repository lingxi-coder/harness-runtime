use super::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;

async fn assert_pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn closed_admission_is_permanent_while_an_earlier_claim_drains() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let retained = lifecycle.clone();
    let claim = lifecycle.claim().unwrap();
    let mut drain = Box::pin(lifecycle.close_and_drain());
    assert_pending(drain.as_mut()).await;
    assert!(retained.claim().is_err());
    assert!(retained.ensure_open().is_err());
    drop(claim);
    assert_eq!(drain.await, BackgroundLaunchDrainReport::default());
    assert_eq!(
        lifecycle.close_and_drain().await,
        BackgroundLaunchDrainReport::default()
    );
    assert!(retained.claim().is_err());
}

#[tokio::test]
async fn cancelled_caller_and_waiter_cannot_hide_a_late_publication_error() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let claim = lifecycle.claim().unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let caller = tokio::spawn({
        let started = started.clone();
        let release = release.clone();
        async move {
            claim
                .run_owned(async move {
                    started.notify_one();
                    release.notified().await;
                    Err::<(), _>(BgForkError::Dispatch("handoff rollback failed".into()))
                })
                .await
        }
    });
    started.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let mut abandoned = Box::pin(lifecycle.close_and_drain());
    assert_pending(abandoned.as_mut()).await;
    drop(abandoned);
    let mut retry = Box::pin(lifecycle.close_and_drain());
    assert_pending(retry.as_mut()).await;
    release.notify_one();
    let report = retry.await;
    assert!(report.unconfirmed_recoveries.is_empty());
    assert!(report
        .diagnostics
        .iter()
        .any(|error| error.contains("handoff rollback failed")));
    assert_eq!(lifecycle.close_and_drain().await, report);
}

#[tokio::test]
async fn completed_panic_is_a_diagnostic_not_an_unresolved_producer() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let error = lifecycle
        .claim()
        .unwrap()
        .run_owned(async {
            panic!("injected publication panic");
            #[allow(unreachable_code)]
            Ok::<_, BgForkError>(())
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("owner panicked"));
    let report = lifecycle.close_and_drain().await;
    assert!(report.unconfirmed_recoveries.is_empty());
    assert_eq!(report.diagnostics.len(), 1);
    assert!(report.diagnostics[0].contains("owner panicked"));
    assert_eq!(lifecycle.close_and_drain().await, report);
}

struct RetryRecovery {
    attempts: AtomicUsize,
    may_recover: AtomicBool,
}

#[async_trait]
impl BackgroundLaunchRecovery for RetryRecovery {
    fn description(&self) -> String {
        "shell handoff job-1".into()
    }

    async fn recover(&self) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.may_recover.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("durable decision unavailable".into())
        }
    }
}

#[tokio::test]
async fn unconfirmed_recovery_retains_owner_until_real_recovery_succeeds() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let recovery = Arc::new(RetryRecovery {
        attempts: AtomicUsize::new(0),
        may_recover: AtomicBool::new(false),
    });
    let weak = Arc::downgrade(&recovery);
    let claim = lifecycle.claim().unwrap();
    let ticket = claim.register_recovery(recovery.clone());
    let error = claim
        .run_owned(async move {
            drop(ticket);
            Err::<(), _>(BgForkError::Dispatch("shell handoff failed".into()))
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("shell handoff failed"));
    let first = lifecycle.close_and_drain().await;
    assert_eq!(recovery.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(first.unconfirmed_recoveries.len(), 1);
    assert!(first.unconfirmed_recoveries[0].contains("durable decision unavailable"));
    assert!(first
        .diagnostics
        .iter()
        .any(|error| error.contains("shell handoff failed")));
    drop(recovery);
    let retained = weak
        .upgrade()
        .expect("unresolved recovery must retain owner");
    retained.may_recover.store(true, Ordering::SeqCst);
    let second = lifecycle.close_and_drain().await;
    assert!(second.unconfirmed_recoveries.is_empty());
    assert_eq!(retained.attempts.load(Ordering::SeqCst), 2);
    assert!(second
        .diagnostics
        .iter()
        .any(|error| error.contains("durable decision unavailable")));
    drop(retained);
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn panic_during_handoff_keeps_recovery_until_ownership_is_confirmed() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let recovery = Arc::new(RetryRecovery {
        attempts: AtomicUsize::new(0),
        may_recover: AtomicBool::new(false),
    });
    let claim = lifecycle.claim().unwrap();
    let ticket = claim.register_recovery(recovery.clone());
    assert!(claim
        .run_owned(async move {
            let _ticket = ticket;
            panic!("injected handoff panic");
            #[allow(unreachable_code)]
            Ok::<_, BgForkError>(())
        })
        .await
        .is_err());
    let report = lifecycle.close_and_drain().await;
    assert_eq!(report.unconfirmed_recoveries.len(), 1);
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.contains("owner panicked")));
    recovery.may_recover.store(true, Ordering::SeqCst);
    let report = lifecycle.close_and_drain().await;
    assert!(report.unconfirmed_recoveries.is_empty());
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.contains("owner panicked")));
}

#[tokio::test]
async fn stale_ticket_cannot_clear_an_unconfirmed_handoff() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let claim = lifecycle.claim().unwrap();
    let ticket = claim.register_recovery(Arc::new(RetryRecovery {
        attempts: AtomicUsize::new(0),
        may_recover: AtomicBool::new(false),
    }));
    drop(claim);
    ticket.confirm_resolved();
    assert_eq!(
        lifecycle
            .close_and_drain()
            .await
            .unconfirmed_recoveries
            .len(),
        1
    );
}

#[tokio::test]
async fn confirmed_handoff_does_not_start_recovery() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let recovery = Arc::new(RetryRecovery {
        attempts: AtomicUsize::new(0),
        may_recover: AtomicBool::new(false),
    });
    let claim = lifecycle.claim().unwrap();
    let ticket = claim.register_recovery(recovery.clone());
    claim
        .run_owned(async move {
            ticket.confirm_resolved();
            Ok(())
        })
        .await
        .unwrap();
    assert!(lifecycle
        .close_and_drain()
        .await
        .unconfirmed_recoveries
        .is_empty());
    assert_eq!(recovery.attempts.load(Ordering::SeqCst), 0);
}

struct ReentrantDropRecovery {
    lifecycle: std::sync::Weak<BackgroundLaunchLifecycle>,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[async_trait]
impl BackgroundLaunchRecovery for ReentrantDropRecovery {
    fn description(&self) -> String {
        "reentrant drop".into()
    }

    async fn recover(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Drop for ReentrantDropRecovery {
    fn drop(&mut self) {
        self.lifecycle.upgrade().unwrap().close();
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
    }
}

fn drain_on_thread(
    lifecycle: Arc<BackgroundLaunchLifecycle>,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Receiver<BackgroundLaunchDrainReport>,
) {
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        sent.send(runtime.block_on(lifecycle.close_and_drain()))
            .unwrap();
    });
    (worker, received)
}

#[test]
fn confirmed_ticket_drops_recovery_outside_lock_and_drain_waits_for_drop() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let claim = lifecycle.claim().unwrap();
    let ticket = claim.register_recovery(Arc::new(ReentrantDropRecovery {
        lifecycle: Arc::downgrade(&lifecycle),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    }));
    let confirm = std::thread::spawn(move || ticket.confirm_resolved());
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    drop(claim);
    let (drainer, report) = drain_on_thread(lifecycle);
    assert!(matches!(
        report.recv_timeout(std::time::Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    release_tx.send(()).unwrap();
    confirm.join().unwrap();
    assert_eq!(
        report
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        BackgroundLaunchDrainReport::default()
    );
    drainer.join().unwrap();
}

#[test]
fn recovered_ticket_drops_recovery_outside_lock_and_drain_waits_for_drop() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let claim = lifecycle.claim().unwrap();
    drop(claim.register_recovery(Arc::new(ReentrantDropRecovery {
        lifecycle: Arc::downgrade(&lifecycle),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    })));
    drop(claim);
    let (drainer, report) = drain_on_thread(lifecycle);
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert!(matches!(
        report.recv_timeout(std::time::Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    release_tx.send(()).unwrap();
    assert_eq!(
        report
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        BackgroundLaunchDrainReport::default()
    );
    drainer.join().unwrap();
}

struct BlockingRecovery {
    attempts: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl BackgroundLaunchRecovery for BlockingRecovery {
    fn description(&self) -> String {
        "blocked handoff".into()
    }

    async fn recover(&self) -> Result<(), String> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        if attempt == 0 {
            Err("still pending".into())
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn cancelled_recovery_waiter_does_not_abort_or_duplicate_the_owned_attempt() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let recovery = Arc::new(BlockingRecovery {
        attempts: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let claim = lifecycle.claim().unwrap();
    let ticket = claim.register_recovery(recovery.clone());
    drop(ticket);
    drop(claim);
    let first = tokio::spawn({
        let lifecycle = lifecycle.clone();
        async move { lifecycle.close_and_drain().await }
    });
    recovery.started.notified().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let second = tokio::spawn({
        let lifecycle = lifecycle.clone();
        async move { lifecycle.close_and_drain().await }
    });
    tokio::task::yield_now().await;
    assert_eq!(recovery.attempts.load(Ordering::SeqCst), 1);
    recovery.release.notify_one();
    recovery.started.notified().await;
    assert_eq!(recovery.attempts.load(Ordering::SeqCst), 2);
    recovery.release.notify_one();
    let report = second.await.unwrap();
    assert!(report.unconfirmed_recoveries.is_empty());
    assert_eq!(recovery.max_active.load(Ordering::SeqCst), 1);
    assert!(report
        .diagnostics
        .iter()
        .any(|error| error.contains("still pending")));
}

#[tokio::test]
async fn concurrent_close_and_claim_have_one_admission_order() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let ready = Arc::new(std::sync::Barrier::new(2));
    let claimant = std::thread::spawn({
        let lifecycle = lifecycle.clone();
        let ready = ready.clone();
        move || {
            ready.wait();
            lifecycle.claim()
        }
    });
    ready.wait();
    lifecycle.close();
    let admitted = claimant.join().unwrap();
    assert!(lifecycle.claim().is_err());
    if let Ok(claim) = admitted {
        let mut drain = Box::pin(lifecycle.close_and_drain());
        assert_pending(drain.as_mut()).await;
        drop(claim);
        assert_eq!(drain.await, BackgroundLaunchDrainReport::default());
    } else {
        assert_eq!(
            lifecycle.close_and_drain().await,
            BackgroundLaunchDrainReport::default()
        );
    }
}
