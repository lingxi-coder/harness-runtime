//! Pool-local admission. A reservation is capacity, not an allocated agent.
//!
//! Ordinary spawns and Fusion panel groups no longer share a core: each pool
//! serves one kind of caller, so admission needs no queue, no headroom
//! reservation and no generation bookkeeping. Ordinary admission is fail-fast
//! and a group waits on tokio's own FIFO-fair semaphore.

use platform_api::panel_pool::PanelAdmissionCancellation as CancellationToken;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// Longest a whole-group reservation waits, independent of the run's own
/// deadline. The effective wait is the earlier of the two.
const GROUP_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    InvalidCount,
    Full,
    Cancelled,
    Deadline,
    Closed,
}

pub(crate) struct CapacityCore {
    semaphore: Arc<Semaphore>,
    max: usize,
}

pub(crate) struct TrackedPoolPermit {
    owned: Option<OwnedSemaphorePermit>,
    core: Arc<CapacityCore>,
    group: Option<Arc<PoolGroupDrain>>,
}

pub(crate) struct PoolGroupDrain {
    pending: AtomicUsize,
    changed: tokio::sync::Notify,
}

impl PoolGroupDrain {
    pub(crate) fn new(count: usize) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(count),
            changed: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait::async_trait]
impl platform_api::panel_pool::PanelPoolDrain for PoolGroupDrain {
    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            if self.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

impl TrackedPoolPermit {
    pub(crate) fn track_group(&mut self, group: Arc<PoolGroupDrain>) {
        assert!(
            self.group.is_none(),
            "physical slot already belongs to a group"
        );
        self.group = Some(group);
    }
}

impl CapacityCore {
    pub(crate) fn new(max: usize) -> Result<Arc<Self>, AdmissionError> {
        if max > Semaphore::MAX_PERMITS {
            return Err(AdmissionError::InvalidCount);
        }
        Ok(Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(max)),
            max,
        }))
    }

    pub(crate) fn max(&self) -> usize {
        self.max
    }

    pub(crate) fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub(crate) fn owns(self: &Arc<Self>, permit: &TrackedPoolPermit) -> bool {
        Arc::ptr_eq(self, &permit.core)
    }

    /// Fail-fast single-slot admission. Nothing but physical occupancy can
    /// refuse it: this pool has no waiters to protect.
    pub(crate) fn acquire_ordinary(self: &Arc<Self>) -> Result<TrackedPoolPermit, AdmissionError> {
        let owned = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::Closed => AdmissionError::Closed,
                tokio::sync::TryAcquireError::NoPermits => AdmissionError::Full,
            })?;
        Ok(TrackedPoolPermit {
            owned: Some(owned),
            core: self.clone(),
            group: None,
        })
    }

    /// Whole-group admission: every slot or none. tokio's semaphore queues
    /// waiters in FIFO order and hands released permits to the front of that
    /// queue, so a group cannot be starved by a later one and there is no
    /// partial start to unwind.
    pub(crate) async fn reserve_group(
        self: &Arc<Self>,
        count: usize,
        overall_deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Vec<TrackedPoolPermit>, AdmissionError> {
        if count == 0 || count > self.max || count > u32::MAX as usize {
            return Err(AdmissionError::InvalidCount);
        }
        let deadline = overall_deadline.min(Instant::now() + GROUP_ADMISSION_TIMEOUT);
        let acquire = self.semaphore.clone().acquire_many_owned(count as u32);
        let mut owned = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(AdmissionError::Cancelled),
            () = tokio::time::sleep_until(deadline) => return Err(AdmissionError::Deadline),
            result = acquire => result.map_err(|_| AdmissionError::Closed)?,
        };
        // Split outside any lock: a tracked permit's destructor is ordinary code.
        let permits = (0..count)
            .map(|_| TrackedPoolPermit {
                owned: Some(owned.split(1).expect("whole group owns every slot")),
                core: self.clone(),
                group: None,
            })
            .collect();
        Ok(permits)
    }
}

impl Drop for TrackedPoolPermit {
    fn drop(&mut self) {
        drop(self.owned.take());
        if let Some(group) = &self.group {
            if group.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                group.changed.notify_waiters();
            }
        }
    }
}

#[cfg(test)]
#[path = "capacity/tests.rs"]
mod tests;
