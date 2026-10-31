//! Wall-clock implementation using `std::time::SystemTime`.

use lingxi_core::host::Clock;
use std::time::SystemTime;

/// Production system clock.
#[derive(Default)]
pub struct SystemClock;

impl SystemClock {
    /// Construct a new `SystemClock`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
