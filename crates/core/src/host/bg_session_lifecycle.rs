//! Admission, owned completion, and recovery for background publication.

use super::BgForkError;
use async_trait::async_trait;
use futures_util::FutureExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

/// A recovery operation owns the resources and evidence needed to determine
/// whether an interrupted handoff is settled. Returning `Ok(())` confirms the
/// ownership transfer or rollback; an error leaves the handoff unresolved.
#[async_trait]
pub trait BackgroundLaunchRecovery: Send + Sync {
    /// Stable description used in shutdown diagnostics.
    fn description(&self) -> String;

    /// Check durable state and complete a safe recovery when possible.
    async fn recover(&self) -> Result<(), String>;
}

/// A completed drain confirms that all accepted producers and the recovery
/// attempts started by this drain have stopped. Historical diagnostics do not
/// prevent cleanup; unresolved recoveries do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundLaunchDrainReport {
    pub diagnostics: Vec<String>,
    pub unconfirmed_recoveries: Vec<String>,
}

struct RecoveryEntry {
    producer_id: u64,
    description: String,
    recovery: Arc<dyn BackgroundLaunchRecovery>,
    retrying: bool,
    last_error: Option<String>,
}

#[derive(Default)]
struct State {
    closing: bool,
    next_producer_id: u64,
    next_recovery_id: u64,
    active: HashSet<u64>,
    // A removed recovery is still dropping outside the lock. Drains must not
    // release dependencies until that destructor actually finishes.
    settling: usize,
    recoveries: HashMap<u64, RecoveryEntry>,
    diagnostics: Vec<String>,
}

/// Closure and claims use one lock. Work accepted before closure transfers its
/// claim into an owned task before the first I/O await, so caller cancellation
/// cannot let shutdown overtake publication or lose its outcome.
#[derive(Default)]
pub struct BackgroundLaunchLifecycle {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

impl BackgroundLaunchLifecycle {
    /// Reject identity reads after the foreground owner closes admission.
    pub fn ensure_open(&self) -> Result<(), BgForkError> {
        if self.state.lock().unwrap_or_else(|p| p.into_inner()).closing {
            Err(BgForkError::Dispatch(
                "foreground session is shutting down".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Admit one launch atomically with respect to permanent closure.
    pub fn claim(self: &Arc<Self>) -> Result<BackgroundLaunchClaim, BgForkError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing {
            return Err(BgForkError::Dispatch(
                "foreground session is shutting down".into(),
            ));
        }
        let id = state.next_producer_id;
        state.next_producer_id = id
            .checked_add(1)
            .ok_or_else(|| BgForkError::Dispatch("background launch identity exhausted".into()))?;
        assert!(state.active.insert(id));
        Ok(BackgroundLaunchClaim {
            lifecycle: self.clone(),
            id,
        })
    }

    /// Synchronously and permanently revoke admission.
    pub fn close(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closing = true;
    }

    fn finish_removal(&self, removed: Option<RecoveryEntry>) {
        if let Some(entry) = removed {
            let description = entry.description.clone();
            // Resource destructors may re-enter this lifecycle. Never run one
            // with its state locked, and always release the settling barrier.
            let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(entry)));
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.settling = state
                .settling
                .checked_sub(1)
                .expect("background recovery settlement underflow");
            if dropped.is_err() {
                state.diagnostics.push(format!(
                    "background recovery {description} resource drop panicked"
                ));
            }
            drop(state);
            self.changed.notify_waiters();
        }
    }

    /// Wait for producers, then attempt each unresolved recovery at most once
    /// for this call. Each attempt runs under an independent owner; dropping a
    /// drain waiter cannot cancel it, and concurrent drains never retry the
    /// same recovery at the same time. Failed or panicked attempts remain
    /// registered, with their resources intact, for a later drain to retry.
    pub async fn close_and_drain(self: &Arc<Self>) -> BackgroundLaunchDrainReport {
        self.close();
        let mut attempted = HashSet::new();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            let retries = {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if !state.active.is_empty()
                    || state.settling != 0
                    || state.recoveries.values().any(|entry| entry.retrying)
                {
                    Vec::new()
                } else {
                    let retries: Vec<_> = state
                        .recoveries
                        .iter_mut()
                        .filter_map(|(&id, entry)| {
                            if attempted.insert(id) {
                                entry.retrying = true;
                                Some((id, entry.recovery.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    if retries.is_empty() {
                        let mut unconfirmed_recoveries: Vec<_> = state
                            .recoveries
                            .values()
                            .map(|entry| match &entry.last_error {
                                Some(error) => format!("{}: {error}", entry.description),
                                None => entry.description.clone(),
                            })
                            .collect();
                        unconfirmed_recoveries.sort();
                        return BackgroundLaunchDrainReport {
                            diagnostics: state.diagnostics.clone(),
                            unconfirmed_recoveries,
                        };
                    }
                    retries
                }
            };

            for (id, recovery) in retries {
                let owner = self.clone();
                tokio::spawn(async move {
                    let result = std::panic::AssertUnwindSafe(recovery.recover())
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err("recovery owner panicked".into()));
                    // The worker's clone must be released before drain can
                    // observe successful settlement and tear dependencies down.
                    drop(recovery);
                    let mut state = owner.state.lock().unwrap_or_else(|p| p.into_inner());
                    let removed = match result {
                        Ok(()) => {
                            let removed = state.recoveries.remove(&id);
                            if removed.is_some() {
                                state.settling += 1;
                            }
                            removed
                        }
                        Err(error) => {
                            if let Some(entry) = state.recoveries.get_mut(&id) {
                                entry.retrying = false;
                                entry.last_error = Some(error.clone());
                                let description = entry.description.clone();
                                state.diagnostics.push(format!(
                                    "background recovery {description} failed: {error}"
                                ));
                            }
                            None
                        }
                    };
                    drop(state);
                    owner.finish_removal(removed);
                    owner.changed.notify_waiters();
                });
            }
            changed.await;
        }
    }
}

/// A launch admitted before closure. Dropping an unstarted claim releases it;
/// `run_owned` instead transfers it to the publication task.
pub struct BackgroundLaunchClaim {
    lifecycle: Arc<BackgroundLaunchLifecycle>,
    id: u64,
}

impl BackgroundLaunchClaim {
    /// Register recovery before initiating a potentially uncertain handoff.
    /// The recovery object must retain the required resources and evidence.
    /// Dropping the returned ticket does not clear the registration.
    pub fn register_recovery(
        &self,
        recovery: Arc<dyn BackgroundLaunchRecovery>,
    ) -> BackgroundRecoveryTicket {
        let description = recovery.description();
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(state.active.contains(&self.id));
        let id = state.next_recovery_id;
        state.next_recovery_id = id.checked_add(1).expect("recovery identity exhausted");
        state.recoveries.insert(
            id,
            RecoveryEntry {
                producer_id: self.id,
                description,
                recovery,
                retrying: false,
                last_error: None,
            },
        );
        BackgroundRecoveryTicket {
            lifecycle: self.lifecycle.clone(),
            id,
            producer_id: self.id,
        }
    }

    /// Keep publication alive independently of the client awaiting its result.
    /// Every failure is recorded before releasing the active claim, including
    /// when the reply receiver has already been dropped.
    pub async fn run_owned<T, F>(self, operation: F) -> Result<T, BgForkError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, BgForkError>> + Send + 'static,
    {
        let (reply, result) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let outcome = std::panic::AssertUnwindSafe(operation)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(BgForkError::Dispatch(
                        "background launch owner panicked".into(),
                    ))
                });
            if let Err(error) = &outcome {
                self.lifecycle
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .diagnostics
                    .push(error.to_string());
            }
            drop(self);
            let _ = reply.send(outcome);
        });
        result
            .await
            .map_err(|_| BgForkError::Dispatch("background launch owner stopped".into()))?
    }
}

impl Drop for BackgroundLaunchClaim {
    fn drop(&mut self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            state.active.remove(&self.id),
            "background launch claim missing"
        );
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

/// Proof obligation for one registered handoff. A dropped ticket deliberately
/// leaves the recovery visible; only an active producer that has actually
/// confirmed settlement can resolve it directly.
pub struct BackgroundRecoveryTicket {
    lifecycle: Arc<BackgroundLaunchLifecycle>,
    id: u64,
    producer_id: u64,
}

impl BackgroundRecoveryTicket {
    /// Confirm verified handoff or rollback while its producer is still active.
    /// A stale ticket cannot clear recovery after the producer has stopped.
    pub fn confirm_resolved(self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let removed = if state.active.contains(&self.producer_id)
            && state
                .recoveries
                .get(&self.id)
                .is_some_and(|entry| entry.producer_id == self.producer_id && !entry.retrying)
        {
            let removed = state.recoveries.remove(&self.id);
            if removed.is_some() {
                state.settling += 1;
            }
            removed
        } else {
            None
        };
        drop(state);
        self.lifecycle.finish_removal(removed);
    }
}

#[cfg(test)]
#[path = "bg_session_lifecycle_tests.rs"]
mod tests;
