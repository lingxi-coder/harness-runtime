use super::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

struct BlockingPrewarm {
    started: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicBool>,
}

impl Future for BlockingPrewarm {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.started.notify_one();
        Poll::Pending
    }
}

impl Drop for BlockingPrewarm {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

fn blocking_prewarm() -> (BlockingPrewarm, Arc<tokio::sync::Notify>, Arc<AtomicBool>) {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    (
        BlockingPrewarm {
            started: started.clone(),
            dropped: dropped.clone(),
        },
        started,
        dropped,
    )
}

async fn assert_pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn shutdown_joins_work_after_the_first_drain_waiter_is_cancelled() {
    let prewarm = StartupPrewarm::default();
    let (work, started, dropped) = blocking_prewarm();
    prewarm.spawn(work);
    started.notified().await;
    let mut abandoned = Box::pin(prewarm.close_and_drain());
    assert_pending(abandoned.as_mut()).await;
    assert!(!dropped.load(Ordering::Acquire));
    drop(abandoned);
    assert_eq!(
        prewarm.close_and_drain().await,
        StartupPrewarmDrainReport::default()
    );
    assert!(dropped.load(Ordering::Acquire));
    assert!(!prewarm.is_running());
    assert_eq!(
        prewarm.close_and_drain().await,
        StartupPrewarmDrainReport::default()
    );
}

#[tokio::test]
async fn close_prevents_late_spawn_from_being_polled() {
    let prewarm = StartupPrewarm::default();
    prewarm.close_and_drain().await;
    let (work, started, dropped) = blocking_prewarm();
    prewarm.spawn(work);
    assert!(dropped.load(Ordering::Acquire));
    assert!(!prewarm.is_running());
    let mut start = Box::pin(started.notified());
    assert_pending(start.as_mut()).await;
}

#[tokio::test]
async fn ordinary_abort_allows_replacement_and_shutdown_joins_both_generations() {
    let prewarm = StartupPrewarm::default();
    let (first, first_started, first_dropped) = blocking_prewarm();
    prewarm.spawn(first);
    first_started.notified().await;
    prewarm.abort_current();
    let (second, second_started, second_dropped) = blocking_prewarm();
    prewarm.spawn(second);
    second_started.notified().await;
    prewarm.close_and_drain().await;
    assert!(first_dropped.load(Ordering::Acquire));
    assert!(second_dropped.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_and_spawn_cannot_resurrect_a_closed_prewarm() {
    let prewarm = Arc::new(StartupPrewarm::default());
    let ready = Arc::new(std::sync::Barrier::new(2));
    let (work, _started, dropped) = blocking_prewarm();
    let runtime = tokio::runtime::Handle::current();
    let spawner = std::thread::spawn({
        let prewarm = prewarm.clone();
        let ready = ready.clone();
        move || {
            let _runtime = runtime.enter();
            ready.wait();
            prewarm.spawn(work);
        }
    });
    ready.wait();
    prewarm.close();
    spawner.join().unwrap();
    prewarm.close_and_drain().await;
    assert!(dropped.load(Ordering::Acquire));
    assert!(!prewarm.is_running());
}

#[tokio::test]
async fn completed_panic_remains_diagnostic_without_blocking_drain() {
    let prewarm = StartupPrewarm::default();
    let started = Arc::new(tokio::sync::Notify::new());
    prewarm.spawn({
        let started = started.clone();
        async move {
            started.notify_one();
            panic!("injected prewarm failure");
        }
    });
    started.notified().await;
    let report = prewarm.close_and_drain().await;
    assert_eq!(report.diagnostics.len(), 1);
    assert!(report.diagnostics[0].contains("startup prewarm failed"));
    assert_eq!(prewarm.close_and_drain().await, report);
}
