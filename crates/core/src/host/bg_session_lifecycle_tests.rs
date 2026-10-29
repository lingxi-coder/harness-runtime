use super::*;
use std::task::Poll;

async fn assert_pending<F: Future>(future: std::pin::Pin<&mut F>) {
    let mut future = future;
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn closed_admission_rejects_new_claims_while_an_earlier_claim_drains() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let claim = lifecycle.claim().unwrap();
    let mut drain = Box::pin(lifecycle.close_and_drain());
    assert_pending(drain.as_mut()).await;
    assert!(lifecycle.claim().is_err());
    assert!(lifecycle.ensure_open().is_err());
    drop(claim);
    drain.await.unwrap();
    lifecycle.close_and_drain().await.unwrap();
    assert!(lifecycle.claim().is_err());
}

#[tokio::test]
async fn cancelled_caller_and_drain_waiter_cannot_release_a_publication_claim() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let claim = lifecycle.claim().unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let caller = tokio::spawn({
        let started = started.clone();
        let release = release.clone();
        let published = published.clone();
        async move {
            claim
                .run_owned(async move {
                    started.notify_one();
                    release.notified().await;
                    published.store(true, std::sync::atomic::Ordering::Release);
                    Ok(())
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
    assert!(!published.load(std::sync::atomic::Ordering::Acquire));
    release.notify_one();
    retry.await.unwrap();
    assert!(published.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn panicked_publication_is_retained_as_a_shutdown_error() {
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
    assert!(lifecycle.close_and_drain().await.is_err());
    assert!(lifecycle.close_and_drain().await.is_err());
}

#[tokio::test]
async fn unresolved_handoff_is_retained_even_when_the_caller_saw_the_error() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let error = lifecycle
        .claim()
        .unwrap()
        .run_owned(async {
            Err::<(), _>(BgForkError::OwnershipUncertain(
                "export fence failed".into(),
            ))
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("export fence failed"));
    assert!(lifecycle
        .close_and_drain()
        .await
        .unwrap_err()
        .to_string()
        .contains("export fence failed"));
    assert!(lifecycle.close_and_drain().await.is_err());
}

#[tokio::test]
async fn ordinary_launch_error_does_not_prevent_shutdown() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    assert!(lifecycle
        .claim()
        .unwrap()
        .run_owned(async { Err::<(), _>(BgForkError::Snapshot("no transcript written".into())) })
        .await
        .is_err());
    lifecycle.close_and_drain().await.unwrap();
}

#[tokio::test]
async fn rejected_duplicate_does_not_poison_an_active_owner_or_shutdown() {
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let first = tokio::spawn({
        let claim = lifecycle.claim().unwrap();
        let started = started.clone();
        let release = release.clone();
        async move {
            claim
                .run_owned(async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(())
                })
                .await
        }
    });
    started.notified().await;
    assert!(lifecycle
        .claim()
        .unwrap()
        .run_owned(async {
            Err::<(), _>(BgForkError::Dispatch(
                "shell transfer already pending".into(),
            ))
        })
        .await
        .is_err());
    let mut drain = Box::pin(lifecycle.close_and_drain());
    assert_pending(drain.as_mut()).await;
    release.notify_one();
    first.await.unwrap().unwrap();
    drain.await.unwrap();
}

#[test]
fn runtime_shutdown_drops_publication_before_releasing_its_claim() {
    struct PendingPublication {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Future for PendingPublication {
        type Output = Result<(), BgForkError>;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<Self::Output> {
            self.started.notify_one();
            Poll::Pending
        }
    }
    impl Drop for PendingPublication {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let lifecycle = Arc::new(BackgroundLaunchLifecycle::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let claim = lifecycle.claim().unwrap();
        drop(tokio::spawn(claim.run_owned(PendingPublication {
            started: started.clone(),
            dropped: dropped.clone(),
        })));
        started.notified().await;
    });
    lifecycle.close();
    runtime.shutdown_background();
    let waiter = tokio::runtime::Runtime::new().unwrap();
    waiter.block_on(async {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            lifecycle.close_and_drain(),
        )
        .await
        .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("before ownership settled"));
    });
    assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
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
        drain.await.unwrap();
    } else {
        lifecycle.close_and_drain().await.unwrap();
    }
}
