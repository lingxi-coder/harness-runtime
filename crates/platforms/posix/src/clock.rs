//! Wall-clock implementation using `std::time::SystemTime`.

use lingxi_core::host::Clock;
use std::time::SystemTime;

/// Production system clock.
#[derive(Default)]
pub struct PosixClock;

impl PosixClock {
    /// Construct a new `PosixClock`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for PosixClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
