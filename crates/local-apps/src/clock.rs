//! The wall clock the service stamps records with.
//!
//! Defined here, not imported from the engine's host abstractions, so this
//! crate does not depend on the engine's core for one method. The engine
//! adapts its platform clock with a thin wrapper; tests pin time with
//! [`crate::test_support::FixedClock`].

use std::time::SystemTime;

/// A source of wall-clock time.
pub trait Clock: Send + Sync {
    /// Current wall-clock time.
    fn now(&self) -> SystemTime;
}

/// The operating system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
