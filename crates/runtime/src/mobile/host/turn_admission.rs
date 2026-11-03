//! Linearize an explicit mobile turn's admission against cancellation.
//!
//! Entries belong to one engine handle and survive until it is dropped: a
//! cancelled command can still be waiting on a foreign executor or on the
//! session transition lock, so an expiry would allow that command to revive.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::ClientError;

#[derive(Default)]
pub(super) struct TurnAdmission {
    entries: Mutex<HashMap<(String, u64), Arc<Mutex<bool>>>>,
}

impl TurnAdmission {
    fn entry(&self, session_id: &str, turn_id: u64) -> Arc<Mutex<bool>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries
            .entry((session_id.to_owned(), turn_id))
            .or_default()
            .clone()
    }

    /// A successful explicit cancel fences even an operation not yet admitted.
    pub(super) fn cancel(&self, session_id: &str, turn_id: u64) {
        let entry = self.entry(session_id, turn_id);
        *entry.lock().unwrap_or_else(|error| error.into_inner()) = true;
    }

    /// Check and install an active owner or durable queue entry atomically.
    /// The closure must be synchronous and must not wait for a transition or
    /// a permission response. Other turn IDs use different locks.
    pub(super) fn with_allowed<T>(
        &self,
        session_id: &str,
        turn_id: Option<u64>,
        action: impl FnOnce() -> Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        let Some(turn_id) = turn_id else {
            return action();
        };
        let entry = self.entry(session_id, turn_id);
        let cancelled = entry.lock().unwrap_or_else(|error| error.into_inner());
        if *cancelled {
            return Err(ClientError::Rejected {
                message: crate::side_chat::TURN_CANCELLED_BEFORE_ADMISSION.into(),
            });
        }
        action()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientError, TurnAdmission};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    const THREAD_TIMEOUT: Duration = Duration::from_secs(5);

    fn assert_cancelled(result: Result<(), ClientError>) {
        match result {
            Err(ClientError::Rejected { message }) => {
                assert_eq!(message, crate::side_chat::TURN_CANCELLED_BEFORE_ADMISSION);
            }
            other => panic!("expected a cancelled admission, got {other:?}"),
        }
    }

    #[test]
    fn explicit_cancel_before_admission_skips_installation() {
        let gate = TurnAdmission::default();
        let installed = AtomicBool::new(false);
        gate.cancel("session-a", 7);

        let result = gate.with_allowed("session-a", Some(7), || {
            installed.store(true, Ordering::SeqCst);
            Ok(())
        });

        assert_cancelled(result);
        assert!(!installed.load(Ordering::SeqCst));
        // A delayed retry for the same explicit ID must remain fenced.
        assert_cancelled(gate.with_allowed("session-a", Some(7), || Ok(())));
    }

    #[test]
    fn cancellation_is_scoped_to_both_session_and_turn_id() {
        let gate = TurnAdmission::default();
        gate.cancel("session-a", 7);

        assert_cancelled(gate.with_allowed("session-a", Some(7), || Ok(())));
        assert_eq!(
            gate.with_allowed("session-a", Some(8), || Ok(8)).unwrap(),
            8
        );
        assert_eq!(
            gate.with_allowed("session-b", Some(7), || Ok(7)).unwrap(),
            7
        );
        assert_eq!(
            gate.with_allowed("session-b", Some(8), || Ok(9)).unwrap(),
            9
        );
    }

    #[test]
    fn absent_turn_id_does_not_inherit_an_explicit_cancel() {
        let gate = TurnAdmission::default();
        gate.cancel("session-a", 7);

        assert_eq!(gate.with_allowed("session-a", None, || Ok(42)).unwrap(), 42);
    }

    #[test]
    fn installation_and_same_key_cancel_are_serialized() {
        let gate = Arc::new(TurnAdmission::default());
        let installed = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let installing_gate = Arc::clone(&gate);
        let installed_by_action = Arc::clone(&installed);
        let install = thread::spawn(move || {
            installing_gate
                .with_allowed("session-a", Some(7), || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(THREAD_TIMEOUT).unwrap();
                    installed_by_action.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
        });
        entered_rx.recv_timeout(THREAD_TIMEOUT).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let cancelling_gate = Arc::clone(&gate);
        let cancel = thread::spawn(move || {
            started_tx.send(()).unwrap();
            cancelling_gate.cancel("session-a", 7);
            finished_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(THREAD_TIMEOUT).unwrap();
        let finished_before_release = finished_rx.recv_timeout(Duration::from_millis(100));
        release_tx.send(()).unwrap();
        install.join().unwrap();
        cancel.join().unwrap();

        assert!(matches!(
            finished_before_release,
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(installed.load(Ordering::SeqCst));
        assert_cancelled(gate.with_allowed("session-a", Some(7), || Ok(())));
    }

    #[test]
    fn held_installation_does_not_block_an_unrelated_key() {
        let gate = Arc::new(TurnAdmission::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held_gate = Arc::clone(&gate);
        let held = thread::spawn(move || {
            held_gate
                .with_allowed("session-a", Some(7), || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(THREAD_TIMEOUT).unwrap();
                    Ok(())
                })
                .unwrap();
        });
        entered_rx.recv_timeout(THREAD_TIMEOUT).unwrap();

        let (finished_tx, finished_rx) = mpsc::channel();
        let other_gate = Arc::clone(&gate);
        let other = thread::spawn(move || {
            other_gate.cancel("session-b", 7);
            let allowed = other_gate.with_allowed("session-a", Some(8), || Ok(8));
            finished_tx.send(allowed).unwrap();
        });
        let unrelated_result = finished_rx.recv_timeout(Duration::from_secs(1));
        // Release first even on timeout, so a regression fails without stranding threads.
        release_tx.send(()).unwrap();
        held.join().unwrap();
        other.join().unwrap();

        assert_eq!(unrelated_result.unwrap().unwrap(), 8);
        assert_cancelled(gate.with_allowed("session-b", Some(7), || Ok(())));
    }
}
