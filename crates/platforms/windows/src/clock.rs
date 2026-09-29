//! Wall-clock implementation using `std::time::SystemTime`.

use lingxi_core::host::Clock;
use std::time::SystemTime;

/// Production system clock.
#[derive(Default)]
pub struct WindowsClock;

impl WindowsClock {
    /// Construct a new `WindowsClock`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for WindowsClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
