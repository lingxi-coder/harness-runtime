//! Compact-warning suppression state.
//!
//! TS ref: `src/services/compact/compactWarningState.ts` (full file).
//!
//! Tracks whether the "context left until autocompact" warning should be
//! suppressed. We suppress immediately after a successful compaction (because
//! accurate token counts are unavailable until the next API response) and
//! clear the suppression at the start of every new compact attempt.
//!
//! TS backs this with a `createStore<boolean>(false)`. The Rust equivalent is
//! a process-wide [`AtomicBool`] so the TUI status line (which renders the
//! warning) and the orchestrator (which suppresses/clears around compaction)
//! observe the same flag without threading a handle through every call.
//!
//! Wiring note: the spec asks for this to be read by the orchestrator
//! app-state so the TUI warning can consult it. That cross-crate wiring lives
//! outside the `compaction` crate and is flagged BLOCKED in the batch report;
//! this module ships the shared state + a process-global accessor so the
//! orchestrator can adopt it without a new type.

use std::sync::atomic::{AtomicBool, Ordering};

/// Suppression flag for the "context left until autocompact" warning.
///
/// Mirrors `compactWarningStore` (`createStore<boolean>(false)`): `true` means
/// the warning is suppressed. Default is `false` (warning shown).
#[derive(Debug)]
pub struct CompactWarningState(AtomicBool);

impl CompactWarningState {
    /// New state with the warning **not** suppressed (TS default `false`).
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Suppress the compact warning. Call after a successful compaction.
    ///
    /// TS `suppressCompactWarning`.
    pub fn suppress(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Clear the suppression. Call at the start of a new compact attempt.
    ///
    /// TS `clearCompactWarningSuppression`.
    pub fn clear_suppression(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// Whether the warning is currently suppressed.
    #[must_use]
    pub fn is_suppressed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for CompactWarningState {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide compact-warning store, mirroring the TS module-level
/// `compactWarningStore` singleton. The orchestrator suppresses/clears this and
/// the TUI status line reads it.
static COMPACT_WARNING_STORE: CompactWarningState = CompactWarningState::new();

/// Suppress the compact warning on the process-global store
/// (TS `suppressCompactWarning`).
pub fn suppress_compact_warning() {
    COMPACT_WARNING_STORE.suppress();
}

/// Clear the compact-warning suppression on the process-global store
/// (TS `clearCompactWarningSuppression`).
pub fn clear_compact_warning_suppression() {
    COMPACT_WARNING_STORE.clear_suppression();
}

/// Whether the process-global compact warning is suppressed.
#[must_use]
pub fn is_compact_warning_suppressed() -> bool {
    COMPACT_WARNING_STORE.is_suppressed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_not_suppressed() {
        let s = CompactWarningState::new();
        assert!(!s.is_suppressed());
        assert!(!CompactWarningState::default().is_suppressed());
    }

    #[test]
    fn suppress_then_clear_round_trip() {
        let s = CompactWarningState::new();
        assert!(!s.is_suppressed());
        s.suppress();
        assert!(s.is_suppressed());
        s.clear_suppression();
        assert!(!s.is_suppressed());
    }

    #[test]
    fn suppress_is_idempotent() {
        let s = CompactWarningState::new();
        s.suppress();
        s.suppress();
        assert!(s.is_suppressed());
        s.clear_suppression();
        s.clear_suppression();
        assert!(!s.is_suppressed());
    }

    #[test]
    fn process_global_round_trip() {
        // Round-trip the singleton, restoring default to avoid cross-test leak.
        clear_compact_warning_suppression();
        assert!(!is_compact_warning_suppressed());
        suppress_compact_warning();
        assert!(is_compact_warning_suppressed());
        clear_compact_warning_suppression();
        assert!(!is_compact_warning_suppressed());
    }
}
