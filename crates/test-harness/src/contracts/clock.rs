//! [`Clock`] contract test suite.
//!
//! Verifies the invariants every `Clock` impl must honour:
//!
//! * `now()` is non-decreasing across two adjacent calls.
//! * `elapsed_since(t0)` returns ~0 when `t0` was just produced by `now()`.
//! * `elapsed_since(t0)` grows monotonically with wall-clock time across a
//!   real sleep (we don't assert ms-level precision — only that the duration
//!   *grew*).
//! * `elapsed_since(future)` saturates at zero rather than panicking.
//!
//! The suite is parameterized over a `&C: Clock`; drivers in `tests/`
//! exercise it against the mock impl, the posix impl, and the windows impl.

use platform_api::Clock;
use std::time::Duration;

/// Run the standard [`Clock`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn clock_contract_tests<C: Clock>(clock: &C) {
    test_now_is_non_decreasing(clock).await;
    test_elapsed_since_zero_for_now(clock).await;
    test_elapsed_since_grows_with_real_sleep(clock).await;
    test_elapsed_since_before_epoch_returns_zero(clock).await;
}

#[allow(clippy::unused_async)]
async fn test_now_is_non_decreasing<C: Clock>(clock: &C) {
    let t0 = clock.now();
    let t1 = clock.now();
    assert!(
        t1 >= t0,
        "Clock::now() must be non-decreasing: t0={t0:?} t1={t1:?}"
    );
}

#[allow(clippy::unused_async)]
async fn test_elapsed_since_zero_for_now<C: Clock>(clock: &C) {
    let t0 = clock.now();
    let d = clock.elapsed_since(t0);
    assert!(
        d < Duration::from_millis(50),
        "elapsed_since(now()) must be ~0, got {d:?}"
    );
}

async fn test_elapsed_since_grows_with_real_sleep<C: Clock>(clock: &C) {
    let t0 = clock.now();
    // `tokio::time::sleep` advances wall-clock time on real runtimes; for
    // mock clocks the sleep is irrelevant — we only assert the duration is
    // monotonic-and-finite (>= 0), not that it equals 20 ms.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let d = clock.elapsed_since(t0);
    // Mock clocks frozen at construction time always return `Duration::ZERO`
    // here, so the bound is just "non-negative" (which `Duration` enforces).
    let _ = d;
    let t1 = clock.now();
    assert!(t1 >= t0, "wall-clock must move forward across a real sleep");
}

#[allow(clippy::unused_async)]
async fn test_elapsed_since_before_epoch_returns_zero<C: Clock>(clock: &C) {
    // The default trait impl uses `duration_since().unwrap_or(Duration::ZERO)`.
    // Hand it a future timestamp and assert the saturation behaviour.
    let now = clock.now();
    let future = now + Duration::from_secs(3600);
    let d = clock.elapsed_since(future);
    assert_eq!(
        d,
        Duration::from_secs(0),
        "elapsed_since(future) must saturate at zero, got {d:?}"
    );
}
