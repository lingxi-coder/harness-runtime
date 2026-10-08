//! Ephemeral safety events attributed to one immutable cost-session entry.
//! Event observations are distinct from settled money and are never persisted.

use lingxi_core::host::model_safety::ModelSafetyObserver;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
pub(crate) struct SafetyStops(AtomicU64);

impl SafetyStops {
    pub(crate) fn snapshot(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    pub(crate) fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
    pub(crate) fn observer(self: &Arc<Self>) -> ModelSafetyObserver {
        let origin = self.clone();
        ModelSafetyObserver::new(move |_| {
            let _ = origin
                .0
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(1))
                });
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::model_safety::ModelSafetyStop;

    #[test]
    fn events_are_not_call_counts_and_origin_authority_is_retained() {
        let original = Arc::new(SafetyStops::default());
        let observer = original.observer();
        observer.record(ModelSafetyStop::Refusal);
        observer.record(ModelSafetyStop::ContentFiltering);
        assert_eq!(original.snapshot(), 2);
        original.reset();
        assert_eq!(original.snapshot(), 0);
        let resumed = Arc::new(SafetyStops::default());
        observer.record(ModelSafetyStop::Refusal);
        assert_eq!(
            original.snapshot(),
            1,
            "same authority can observe events after clear"
        );
        assert_eq!(
            resumed.snapshot(),
            0,
            "fresh resume owner cannot inherit old observations"
        );
    }
}
