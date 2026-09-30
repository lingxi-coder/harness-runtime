//! Regression tests for violation store test.

use sandbox::violation_store::{
    SandboxViolationEvent, SandboxViolationKind, SandboxViolationStore, SANDBOX_VIOLATION_STORE_CAP,
};

fn make_event(idx: u64) -> SandboxViolationEvent {
    SandboxViolationEvent {
        timestamp_ms: idx,
        command: format!("cmd-{idx}"),
        violation_type: SandboxViolationKind::FileWrite,
        message: format!("blocked write #{idx}"),
    }
}

#[tokio::test]
async fn record_and_snapshot_in_order() {
    let store = SandboxViolationStore::new();
    store.record(make_event(1)).await;
    store.record(make_event(2)).await;
    store.record(make_event(3)).await;
    let snap = store.snapshot().await;
    assert_eq!(snap.len(), 3);
    assert_eq!(snap[0].timestamp_ms, 1);
    assert_eq!(snap[2].timestamp_ms, 3);
}

#[tokio::test]
async fn clear_drains_buffer() {
    let store = SandboxViolationStore::new();
    store.record(make_event(1)).await;
    store.record(make_event(2)).await;
    store.clear().await;
    assert_eq!(store.len().await, 0);
    assert!(store.snapshot().await.is_empty());
}

#[tokio::test]
async fn capacity_evicts_oldest_first() {
    // Insert cap + 5 events; verify oldest 5 dropped.
    let store = SandboxViolationStore::new();
    for i in 0..(SANDBOX_VIOLATION_STORE_CAP as u64 + 5) {
        store.record(make_event(i)).await;
    }
    assert_eq!(store.len().await, SANDBOX_VIOLATION_STORE_CAP);
    let snap = store.snapshot().await;
    // Oldest retained event should have timestamp 5 (events 0..5 evicted).
    assert_eq!(snap[0].timestamp_ms, 5);
    assert_eq!(
        snap[snap.len() - 1].timestamp_ms,
        SANDBOX_VIOLATION_STORE_CAP as u64 + 4
    );
}

#[tokio::test]
async fn capacity_constant_is_one_thousand() {
    assert_eq!(SANDBOX_VIOLATION_STORE_CAP, 1000);
}
