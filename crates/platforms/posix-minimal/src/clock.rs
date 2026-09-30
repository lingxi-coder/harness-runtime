//! Posix [`Clock`] backed by `std::time::SystemTime`.

use lingxi_core::host::Clock;
use std::time::SystemTime;

/// Standard system clock (real wall-clock time).
#[derive(Default)]
pub struct PosixClock;

impl PosixClock {
    /// Construct.
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
