//! Per-turn delivery exclusion for the mobile recovery journal.
//!
//! This module does not queue events or call a listener. A caller must acquire
//! the gate before deciding ownership or committing an event, then release all
//! owner and store locks before awaiting a foreign callback. An armed guard
//! must be kept alive by a completion task until the full batch has been
//! delivered; dropping it earlier poisons the turn's gate.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::turn_durability::DurableTurnStore;

type TurnKey = (String, u64);

struct GateEntry {
    gate: Weak<TurnDeliveryGate>,
    // Retain failure even if the last strong gate reference disappears. A
    // replacement gate in the same store must not silently clear a gap.
    poisoned: Arc<AtomicBool>,
}

/// The registry belongs to one shared store. Healthy, unused gates are weak;
/// poisoned entries remain until the store itself is released.
#[derive(Default)]
pub(crate) struct TurnDeliveryGates {
    entries: Arc<Mutex<HashMap<TurnKey, GateEntry>>>,
}

impl TurnDeliveryGates {
    pub(super) fn get(
        &self,
        store: Arc<DurableTurnStore>,
        session_id: &str,
        turn_id: u64,
    ) -> Arc<TurnDeliveryGate> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, entry| {
            entry.gate.strong_count() != 0 || entry.poisoned.load(Ordering::Acquire)
        });

        let key = (session_id.to_owned(), turn_id);
        if let Some(gate) = entries.get(&key).and_then(|entry| entry.gate.upgrade()) {
            return gate;
        }
        let poisoned = entries
            .get(&key)
            .map(|entry| entry.poisoned.clone())
            .unwrap_or_default();
        let gate = Arc::new(TurnDeliveryGate {
            // The registry only keeps a Weak to this gate. Holding the store
            // here makes an in-flight guard or waiter keep the shared-store
            // registry alive, so a second handle cannot create another gate.
            _store: store,
            mutex: Arc::new(AsyncMutex::new(())),
            poisoned: poisoned.clone(),
            entries: self.entries.clone(),
        });
        entries.insert(
            key,
            GateEntry {
                gate: Arc::downgrade(&gate),
                poisoned,
            },
        );
        gate
    }
}

/// One serialization point for a store, session and durable turn id.
pub(crate) struct TurnDeliveryGate {
    _store: Arc<DurableTurnStore>,
    mutex: Arc<AsyncMutex<()>>,
    poisoned: Arc<AtomicBool>,
    entries: Arc<Mutex<HashMap<TurnKey, GateEntry>>>,
}

/// Delivery was interrupted after a batch could have been committed. The
/// current source must stop advancing this turn's cursor and surface failure.
/// Poison persists for the shared store's lifetime. Recovery requires retiring
/// the old source and in-flight tasks, isolating any late foreign callbacks,
/// then reopening the store with a new listener and replaying from the client
/// cursor. This gate cannot establish that isolation for the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnDeliveryPoisoned;

impl fmt::Display for TurnDeliveryPoisoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("mobile turn delivery was interrupted; reconnect and replay the turn")
    }
}

impl std::error::Error for TurnDeliveryPoisoned {}

impl TurnDeliveryGate {
    /// Acquire the only delivery slot without holding any owner/store lock.
    /// A waiter rechecks poison after acquisition because an earlier guard may
    /// have failed while it was waiting. This mutex is not runtime-affine.
    pub(crate) async fn lock(self: &Arc<Self>) -> Result<TurnDeliveryGuard, TurnDeliveryPoisoned> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(TurnDeliveryPoisoned);
        }
        let guard = self.mutex.clone().lock_owned().await;
        if self.poisoned.load(Ordering::Acquire) {
            return Err(TurnDeliveryPoisoned);
        }
        Ok(TurnDeliveryGuard {
            gate: self.clone(),
            _guard: guard,
            armed: false,
        })
    }
}

/// An owned gate slot that can be moved into a spawned completion task.
/// Dropping an unarmed guard is safe because no event has been committed.
pub(crate) struct TurnDeliveryGuard {
    gate: Arc<TurnDeliveryGate>,
    _guard: OwnedMutexGuard<()>,
    armed: bool,
}

impl TurnDeliveryGuard {
    /// Arm before any mutation that might commit a batch. From this point,
    /// cancellation, panic or an ambiguous write failure poisons the gate.
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }

    /// Release after every callback in the committed batch has returned. This
    /// can also release an armed slot when the caller can prove no mutation
    /// committed; an ambiguous write error is not such proof.
    pub(crate) fn complete(mut self) {
        self.armed = false;
    }
}

impl Drop for TurnDeliveryGuard {
    fn drop(&mut self) {
        if self.armed {
            // This runs before the owned mutex guard is dropped. The next
            // waiter therefore sees poison before it can commit another seq.
            // Synchronize with registry cleanup too: it must not observe the
            // last gate reference gone while missing the poison marker.
            let _entries = self
                .gate
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.gate.poisoned.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::Duration;

    use super::*;

    const DEADLINE: Duration = Duration::from_secs(10);

    async fn bounded<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(DEADLINE, future)
            .await
            .expect("delivery test timed out")
    }

    fn store(root: &std::path::Path) -> Arc<DurableTurnStore> {
        DurableTurnStore::open_shared(root.join("turns")).expect("open shared store")
    }

    #[tokio::test]
    async fn abandoned_armed_guard_poison_survives_gate_recreation() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let registry = TurnDeliveryGates::default();
        let gate = registry.get(store.clone(), "session", 1);
        let mut guard = gate.lock().await.unwrap();
        guard.arm();
        drop(guard);
        drop(gate);

        let replacement = registry.get(store.clone(), "session", 1);
        assert!(matches!(
            replacement.lock().await,
            Err(TurnDeliveryPoisoned)
        ));
        drop(replacement);
        let other = registry.get(store, "session", 2);
        assert!(other.lock().await.is_ok());
        assert_eq!(registry.entries.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn healthy_guards_and_unused_entries_can_be_released() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let registry = TurnDeliveryGates::default();
        let first = registry.get(store.clone(), "session", 1);

        // Dropping before a commit does not poison. A fully completed batch
        // likewise permits the next producer to use the same slot.
        drop(first.lock().await.unwrap());
        let mut guard = first.lock().await.unwrap();
        guard.arm();
        guard.complete();
        assert!(first.lock().await.is_ok());

        drop(first);
        let second = registry.get(store, "session", 2);
        assert_eq!(registry.entries.lock().unwrap().len(), 1);
        assert!(second.lock().await.is_ok());
    }

    #[tokio::test]
    async fn different_sessions_and_turns_do_not_share_a_slot() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let first = store.delivery_gate("session-a", 1).unwrap();
        let different_session = store.delivery_gate("session-b", 1).unwrap();
        let different_turn = store.delivery_gate("session-a", 2).unwrap();

        let mut first_guard = first.lock().await.unwrap();
        first_guard.arm();
        let session_guard = different_session.lock().await.unwrap();
        let turn_guard = different_turn.lock().await.unwrap();
        first_guard.complete();
        session_guard.complete();
        turn_guard.complete();
    }

    #[tokio::test]
    async fn aborting_an_armed_task_fails_closed_for_waiters() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let gate = store.delivery_gate("session", 1).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task_gate = gate.clone();
        let task = tokio::spawn(async move {
            let mut guard = task_gate.lock().await.unwrap();
            guard.arm();
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
            guard.complete();
        });
        bounded(ready_rx).await.unwrap();

        let waiting_gate = gate.clone();
        let waiter = tokio::spawn(async move { waiting_gate.lock().await });
        task.abort();
        assert!(bounded(task).await.unwrap_err().is_cancelled());
        assert!(matches!(
            bounded(waiter).await.unwrap(),
            Err(TurnDeliveryPoisoned)
        ));
        assert!(matches!(gate.lock().await, Err(TurnDeliveryPoisoned)));
    }

    #[tokio::test]
    async fn panicking_delivery_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let gate = store.delivery_gate("session", 1).unwrap();
        let task_gate = gate.clone();
        let task = tokio::spawn(async move {
            let mut guard = task_gate.lock().await.unwrap();
            guard.arm();
            panic!("simulated callback panic");
        });

        assert!(bounded(task).await.unwrap_err().is_panic());
        assert!(matches!(gate.lock().await, Err(TurnDeliveryPoisoned)));
    }

    #[tokio::test]
    async fn completion_task_can_outlive_its_aborted_producer() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let gate = store.delivery_gate("session", 1).unwrap();
        let (delivery_ready_tx, delivery_ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let producer_gate = gate.clone();
        let producer = tokio::spawn(async move {
            let mut guard = producer_gate.lock().await.unwrap();
            guard.arm();
            // In the host, durable commit and this spawn must be in the same
            // poll, with no await between them.
            let delivery = tokio::spawn(async move {
                delivery_ready_tx.send(()).unwrap();
                release_rx.await.unwrap();
                guard.complete();
            });
            delivery.await.unwrap();
        });
        bounded(delivery_ready_rx).await.unwrap();
        producer.abort();
        assert!(bounded(producer).await.unwrap_err().is_cancelled());
        assert!(gate.mutex.try_lock().is_err());

        let waiting_gate = gate.clone();
        let waiter = tokio::spawn(async move { waiting_gate.lock().await });
        release_tx.send(()).unwrap();
        bounded(waiter).await.unwrap().unwrap().complete();
        assert!(gate.lock().await.is_ok());
    }

    #[test]
    fn one_slot_is_shared_across_runtimes_and_keeps_store_alive() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let store_weak = Arc::downgrade(&store);
        let first = store.delivery_gate("session", 1).unwrap();
        let first_thread_gate = first.clone();
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let first_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let guard = first_thread_gate.lock().await.unwrap();
                locked_tx.send(()).unwrap();
                release_rx.await.unwrap();
                guard.complete();
            });
        });
        locked_rx.recv_timeout(DEADLINE).unwrap();
        drop(store);
        assert!(store_weak.upgrade().is_some());

        let reopened = DurableTurnStore::open_shared(temp.path().join("turns")).unwrap();
        let second = reopened.delivery_gate("session", 1).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let second_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                assert!(second.mutex.try_lock().is_err());
                waiting_tx.send(()).unwrap();
                let guard = second.lock().await.unwrap();
                entered_tx.send(()).unwrap();
                guard.complete();
            });
        });
        waiting_rx.recv_timeout(DEADLINE).unwrap();
        assert!(matches!(entered_rx.try_recv(), Err(TryRecvError::Empty)));
        release_tx.send(()).unwrap();
        entered_rx.recv_timeout(DEADLINE).unwrap();
        first_thread.join().unwrap();
        second_thread.join().unwrap();
        drop(first);
        drop(reopened);
        assert!(store_weak.upgrade().is_none());
    }
}
