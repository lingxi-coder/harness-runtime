//! Host-owned, consuming capacity transfers. These tokens cannot be supplied
//! through serialized tool input or copied through subagent inheritance.

use std::any::Any;
use std::sync::Arc;

/// Cancellation used by the host admission seam. Re-export the existing
/// neutral API dependency so pool implementations need no new dependency.
pub use tokio_util::sync::CancellationToken as PanelAdmissionCancellation;

/// One physical pool slot. The receiving host must validate its concrete
/// payload and originating pool before spawning, without acquiring again.
pub struct PanelPoolPermit(Box<dyn Any + Send>);

impl PanelPoolPermit {
    /// Wrap a host's RAII capacity owner. This does not authorize an allocation
    /// in a different pool, even when both pools use the same owner type.
    pub fn new<T: Any + Send>(permit: T) -> Self {
        Self(Box::new(permit))
    }

    /// Consume the token. A mismatched payload is dropped, returning its
    /// capacity to the original host rather than leaking it on rejection.
    pub fn into_inner<T: Any + Send>(self) -> Option<T> {
        self.0.downcast::<T>().ok().map(|permit| *permit)
    }
}

impl std::fmt::Debug for PanelPoolPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelPoolPermit").finish_non_exhaustive()
    }
}

/// All panel slots acquired atomically by one host. Dropping an unconsumed
/// bundle releases every slot; transferring it cannot duplicate capacity.
pub struct PanelPoolLease {
    permits: Vec<PanelPoolPermit>,
    drain: Option<Arc<dyn PanelPoolDrain>>,
}

/// Observe actual producer destruction without retaining any physical slot.
/// Dropping a waiter must not discard the host's ongoing cancellation work.
#[async_trait::async_trait]
pub trait PanelPoolDrain: Send + Sync {
    async fn wait(&self);
}

impl std::fmt::Debug for PanelPoolLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelPoolLease")
            .field("permits", &self.permits.len())
            .field("has_producer_drain", &self.drain.is_some())
            .finish()
    }
}

impl PanelPoolLease {
    pub fn new(permits: Vec<PanelPoolPermit>) -> Self {
        Self {
            permits,
            drain: None,
        }
    }

    pub fn with_drain(permits: Vec<PanelPoolPermit>, drain: Arc<dyn PanelPoolDrain>) -> Self {
        Self {
            permits,
            drain: Some(drain),
        }
    }

    pub fn len(&self) -> usize {
        self.permits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.permits.is_empty()
    }

    pub fn into_permits(self) -> Vec<PanelPoolPermit> {
        self.permits
    }

    pub fn into_parts(self) -> (Vec<PanelPoolPermit>, Option<Arc<dyn PanelPoolDrain>>) {
        (self.permits, self.drain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn bundle_transfer_and_rejected_payload_release_once() {
        let released = Arc::new(AtomicUsize::new(0));
        let lease = PanelPoolLease::new(
            (0..3)
                .map(|_| PanelPoolPermit::new(CountDrop(released.clone())))
                .collect(),
        );
        assert_eq!(lease.len(), 3);
        let mut permits = lease.into_permits();
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert!(permits.pop().unwrap().into_inner::<u64>().is_none());
        assert_eq!(released.load(Ordering::SeqCst), 1);
        let owner = permits.pop().unwrap().into_inner::<CountDrop>().unwrap();
        drop(permits);
        assert_eq!(released.load(Ordering::SeqCst), 2);
        drop(owner);
        assert_eq!(released.load(Ordering::SeqCst), 3);
    }
}
