//! `MockClock` — virtual wall-clock for deterministic tests.

#![allow(clippy::unwrap_used)]

use lingxi_core::host::Clock;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Deterministic [`Clock`] implementation backed by an interior-mutable
/// `SystemTime`. Tests advance the clock explicitly via [`Self::advance`].
pub struct MockClock {
    now: Mutex<SystemTime>,
}

impl MockClock {
    /// Construct a clock fixed at `seconds_since_epoch` past the Unix epoch.
    #[must_use]
    pub fn at(seconds_since_epoch: u64) -> Self {
        Self {
            now: Mutex::new(UNIX_EPOCH + Duration::from_secs(seconds_since_epoch)),
        }
    }

    /// Advance the virtual clock by `d`.
    ///
    /// # Panics
    /// Panics if the internal time mutex is poisoned.
    pub fn advance(&self, d: Duration) {
        let mut t = self.now.lock().unwrap();
        *t += d;
    }
}

impl Clock for MockClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().unwrap()
    }
}
