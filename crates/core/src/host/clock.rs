//! Wall-clock abstraction. Engine code calls this trait instead of
//! `SystemTime::now()` directly so tests can inject a virtual clock.

use std::time::{Duration, SystemTime};

/// Source of wall-clock time.
///
/// Implementations live in platform crates (real clock) and `test-harness`
/// (deterministic clock).
pub trait Clock: Send + Sync {
    /// Current wall-clock time. May not be monotonic across sleep on mobile;
    /// callers needing monotonicity should track relative durations from a
    /// fixed `Instant` instead. See spec §1.4.1 hidden assumptions.
    fn now(&self) -> SystemTime;

    /// Convenience: `now().duration_since(earlier).unwrap_or(Duration::ZERO)`.
    fn elapsed_since(&self, earlier: SystemTime) -> Duration {
        self.now().duration_since(earlier).unwrap_or(Duration::ZERO)
    }
}
