//! Lock-free analytics killswitch.
//!
//! When activated, [`crate::bus::AnalyticsBus`] silently drops events without
//! invoking sinks. Used by the support team to disable telemetry per-org via
//! a feature flag without redeploying.

use std::sync::atomic::{AtomicBool, Ordering};

/// Atomic boolean flag toggling analytics off.
///
/// Cheap to read on the hot path — uses `Acquire` / `Release` ordering to
/// publish the activation across threads without a mutex.
pub struct Killswitch {
    active: AtomicBool,
}

impl Killswitch {
    /// Construct a killswitch in the inactive state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
        }
    }

    /// Returns `true` once [`Self::activate`] has been called.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Activate the killswitch. Idempotent — subsequent calls are no-ops.
    pub fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }
}

impl Default for Killswitch {
    fn default() -> Self {
        Self::new()
    }
}
