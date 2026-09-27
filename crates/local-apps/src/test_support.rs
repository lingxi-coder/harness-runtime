//! Deterministic helpers for local-apps tests (unit, integration, and the
//! engine's wiring tests).

use platform_api::Clock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A [`Clock`] pinned to a settable epoch-milliseconds value.
#[derive(Debug)]
pub struct FixedClock {
    ms: AtomicU64,
}

impl FixedClock {
    /// Clock reading `start_ms` epoch milliseconds until advanced.
    #[must_use]
    pub fn new(start_ms: u64) -> Self {
        Self {
            ms: AtomicU64::new(start_ms),
        }
    }

    /// Move the clock forward by `delta_ms` milliseconds.
    pub fn advance_ms(&self, delta_ms: u64) {
        self.ms.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.ms.load(Ordering::SeqCst))
    }
}
