//! Loom-driven single-flight invariant: concurrent reactive + proactive
//! `refresh()` invocations with the same `prev_token_hash` MUST collapse to
//! exactly ONE HTTP call.
//!
//! Spec §6 line 595, §8 M3-04 phase 8 — THE acceptance criterion.
//! Per v3 §32.7 hotspot list — OAuth single-flight is one of the four loom
//! invariants explicitly listed.
//!
//! Build/run:
//!   `RUSTFLAGS="--cfg loom" cargo test -p lingxi-anthropic-oauth --test refresh_single_flight_test`
//!
//! This file is gated behind `#[cfg(loom)]` so it does NOT compile in normal
//! `cargo test` runs (loom replaces std primitives with model-checking variants).

#![cfg(loom)]

use loom::sync::atomic::{AtomicU32, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// Stripped-down model of `AuthState` + `RefreshDriver::refresh`.
///
/// We don't run the real tokio code under loom (loom doesn't support tokio
/// scheduling). Instead, we model the invariant: a shared `Mutex<()>` for the
/// critical section, a shared atomic for `token_version` (analogue of
/// `TokenHash`), and a shared atomic for `http_calls`. The `refresh` analogue
/// performs: lock → double-check → (if check passes) increment http_calls +
/// bump token_version → unlock.
struct ModelAuthState {
    refresh_lock: Mutex<()>,
    token_version: AtomicU32,
    http_calls: AtomicU32,
}

impl ModelAuthState {
    fn new() -> Self {
        Self {
            refresh_lock: Mutex::new(()),
            token_version: AtomicU32::new(1),
            http_calls: AtomicU32::new(0),
        }
    }

    /// The single-flight refresh under model. Returns `()` on success.
    fn refresh(&self, prev_version: u32) {
        let _guard = self.refresh_lock.lock().unwrap();
        // Double-check after acquire: if version changed, another task
        // refreshed while we were waiting on the lock — bail without HTTP call.
        if self.token_version.load(Ordering::SeqCst) != prev_version {
            return;
        }
        // Otherwise: simulate the HTTP call + token rotation.
        self.http_calls.fetch_add(1, Ordering::SeqCst);
        self.token_version.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn concurrent_refresh_collapses_to_one_http_call() {
    loom::model(|| {
        let state = Arc::new(ModelAuthState::new());
        let prev = state.token_version.load(Ordering::SeqCst);

        let s1 = state.clone();
        let s2 = state.clone();
        let t1 = thread::spawn(move || s1.refresh(prev));
        let t2 = thread::spawn(move || s2.refresh(prev));
        t1.join().unwrap();
        t2.join().unwrap();

        // Across ALL schedules loom explores, exactly one HTTP call escaped.
        assert_eq!(state.http_calls.load(Ordering::SeqCst), 1);
        // And the token rotated exactly once.
        assert_eq!(state.token_version.load(Ordering::SeqCst), prev + 1);
    });
}

#[test]
fn three_concurrent_refreshes_collapse_to_one_http_call() {
    loom::model(|| {
        let state = Arc::new(ModelAuthState::new());
        let prev = state.token_version.load(Ordering::SeqCst);

        let s1 = state.clone();
        let s2 = state.clone();
        let s3 = state.clone();
        let t1 = thread::spawn(move || s1.refresh(prev));
        let t2 = thread::spawn(move || s2.refresh(prev));
        let t3 = thread::spawn(move || s3.refresh(prev));
        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        assert_eq!(state.http_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.token_version.load(Ordering::SeqCst), prev + 1);
    });
}

#[test]
fn reactive_with_stale_hash_after_proactive_makes_zero_extra_calls() {
    // Models: proactive refresh rotates the token; reactive arrives later with
    // the stale prev_hash and double-checks out — no second HTTP call.
    loom::model(|| {
        let state = Arc::new(ModelAuthState::new());
        let initial_version = state.token_version.load(Ordering::SeqCst);

        // T1: proactive refresh (rotates).
        let s1 = state.clone();
        let t1 = thread::spawn(move || s1.refresh(initial_version));

        // T2: reactive refresh arriving with the SAME initial version. After
        // T1 rotates, T2's double-check sees version != initial → returns.
        let s2 = state.clone();
        let t2 = thread::spawn(move || s2.refresh(initial_version));

        t1.join().unwrap();
        t2.join().unwrap();

        // Only one HTTP call regardless of which thread acquired first.
        assert_eq!(state.http_calls.load(Ordering::SeqCst), 1);
    });
}
