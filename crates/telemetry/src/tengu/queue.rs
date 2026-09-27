//! `lingxi_queue_*` event name constants — the observability twin of
//! claude-code's `recordQueueOperation` / `logOperation` operation log.
//!
//! These are NOT added to the count-locked `ALL_EVENT_NAMES` /
//! `tengu_events.json` fixture (that snapshot mirrors an OLDER claude event
//! set; adding entries would break the byte-parity lock). They are also NOT
//! `tengu_*` events — they are LingXi-native `lingxi_queue_*` observability
//! events, mirroring how `tengu::workflow::NAMES` is kept apart.
//!
//! The string values are duplicated as `&str` literals in
//! `msgqueue::telemetry_recorder` (the actual emit site, which keeps msgqueue
//! free of a hard `telemetry` crate dependency). The constants here are the
//! single string-lock source; `string_lock_matches_telemetry_constants` in
//! `msgqueue/src/telemetry_recorder.rs` is the cross-crate guard that the two
//! never drift.

/// `lingxi_queue_enqueued` — a command was added to the queue.
pub const ENQUEUED: &str = "lingxi_queue_enqueued";
/// `lingxi_queue_dequeued` — a command was popped from the queue.
pub const DEQUEUED: &str = "lingxi_queue_dequeued";
/// `lingxi_queue_removed` — a command was explicitly removed (e.g. cancellation).
pub const REMOVED: &str = "lingxi_queue_removed";
/// `lingxi_queue_cleared` — the queue was cleared.
pub const CLEARED: &str = "lingxi_queue_cleared";

/// All 4 queue-operation telemetry event names (string-lock only, NOT in
/// `ALL_EVENT_NAMES`).
pub const NAMES: &[&str] = &[ENQUEUED, DEQUEUED, REMOVED, CLEARED];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name in the queue `NAMES` registry must be a byte-exact
    /// `lingxi_queue_*` string.
    #[test]
    fn all_names_are_lingxi_queue_prefixed() {
        for &name in NAMES {
            assert!(
                name.starts_with("lingxi_queue_"),
                "queue event name {name:?} does not start with 'lingxi_queue_'"
            );
        }
    }

    #[test]
    fn constants_locked() {
        assert_eq!(ENQUEUED, "lingxi_queue_enqueued");
        assert_eq!(DEQUEUED, "lingxi_queue_dequeued");
        assert_eq!(REMOVED, "lingxi_queue_removed");
        assert_eq!(CLEARED, "lingxi_queue_cleared");
    }

    #[test]
    fn names_slice_length_4() {
        assert_eq!(NAMES.len(), 4);
    }
}
