//! [`RuntimeSpawner`] contract test suite.
//!
//! Verifies the invariants every [`RuntimeSpawner`] impl must honour:
//!
//! * `spawn` returns a handle and the future actually runs to completion.
//! * `cancel(handle)` after the task already finished is non-fatal —
//!   implementations may return `Ok(())` or
//!   [`RuntimeError::NotFound`] but MUST NOT panic or hang.
//! * Many concurrent spawns all run (no starvation, no FIFO blocking).
//! * `sleep(0)` returns promptly.
//!
//! The suite is parameterized over a `&R: RuntimeSpawner`; drivers in
//! `tests/` exercise it against the mock, posix, and windows impls.

use lingxi_core::host::{RuntimeError, RuntimeSpawner};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Run the standard [`RuntimeSpawner`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn runtime_spawner_contract_tests<R: RuntimeSpawner>(rt: &R) {
    test_spawn_runs_future_to_completion(rt).await;
    test_cancel_after_completion_is_non_fatal(rt).await;
    test_spawn_many_does_not_starve(rt).await;
    test_sleep_zero_returns_promptly(rt).await;
}

async fn test_spawn_runs_future_to_completion<R: RuntimeSpawner>(rt: &R) {
    let counter = Arc::new(AtomicU32::new(0));
    let c2 = counter.clone();
    let handle = rt
        .spawn(
            "contract-spawn-runs",
            Box::pin(async move {
                c2.store(1, Ordering::SeqCst);
            }),
        )
        .await
        .expect("spawn must succeed");
    // Give the task time to run.
    for _ in 0..50 {
        if counter.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "spawned task must run to completion"
    );
    let _ = rt.cancel(&handle).await;
}

async fn test_cancel_after_completion_is_non_fatal<R: RuntimeSpawner>(rt: &R) {
    let handle = rt
        .spawn("contract-cancel-done", Box::pin(async {}))
        .await
        .expect("spawn ok");
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Either Ok or `NotFound` is acceptable — the contract only forbids
    // panics / hangs / cryptic Internal errors.
    let r = rt.cancel(&handle).await;
    match r {
        Ok(()) | Err(RuntimeError::NotFound(_)) => {}
        other => panic!("cancel of finished task must be Ok or NotFound, got {other:?}"),
    }
}

async fn test_spawn_many_does_not_starve<R: RuntimeSpawner>(rt: &R) {
    let counter = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for i in 0..8 {
        let c = counter.clone();
        handles.push(
            rt.spawn(
                "contract-spawn-many",
                Box::pin(async move {
                    let _ = i;
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await
            .expect("spawn"),
        );
    }
    // Wait up to ~1 s for all 8 to finish.
    for _ in 0..100 {
        if counter.load(Ordering::SeqCst) == 8 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        8,
        "all 8 spawned tasks must complete"
    );
    for h in handles {
        let _ = rt.cancel(&h).await;
    }
}

async fn test_sleep_zero_returns_promptly<R: RuntimeSpawner>(rt: &R) {
    let start = tokio::time::Instant::now();
    rt.sleep(Duration::from_millis(0)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "sleep(0) must return promptly, took {elapsed:?}"
    );
}
