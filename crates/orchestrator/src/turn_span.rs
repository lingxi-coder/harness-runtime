//! Per-turn tallies for the `/loop` no-op fold.
//!
//! Claude Code decides whether a `/loop` tick was quiet by walking the
//! transcript span between the last `scheduled_task_fire{cronKind:"loop"}`
//! anchor and the end of the transcript, vetoing on what it finds there. LingXi
//! delivers a wakeup as exactly ONE queued command, so that span is one turn —
//! and the same facts can be counted as they happen instead of being recovered
//! afterwards from a transcript this port does not hold in memory.
//!
//! The tally is reset at the start of every turn and read once at its
//! completion edge (`apps/bridge-server/src/driver.rs`).

use std::sync::atomic::{AtomicU32, Ordering};

/// Live counters for the turn in flight.
#[derive(Debug, Default)]
pub struct TurnSpanTally {
    tool_uses: AtomicU32,
    messages: AtomicU32,
    denials: AtomicU32,
    aborts: AtomicU32,
    compactions: AtomicU32,
}

/// One read of a [`TurnSpanTally`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnSpanCounts {
    /// `tool_use` blocks the assistant emitted this turn (the oracle's `c.size`).
    pub tool_uses: u32,
    /// Messages appended this turn (the oracle's `span_len`).
    pub messages: u32,
    /// Tool calls DENIED this turn — everything but the two abort kinds.
    pub denials: u32,
    /// Tool calls interrupted or cancelled this turn.
    pub aborts: u32,
    /// Compact boundaries published this turn.
    pub compactions: u32,
}

impl TurnSpanTally {
    /// Start a fresh span. Called at every turn's start, not only loop ticks —
    /// a stale count must never decide the next tick.
    pub fn reset(&self) {
        for counter in self.counters() {
            counter.store(0, Ordering::Relaxed);
        }
    }

    /// Count the `tool_use` blocks of one assistant response, and the messages
    /// that response contributes: itself plus one `tool_result` per call.
    pub fn note_assistant_response(&self, tool_uses: usize) {
        let calls = u32::try_from(tool_uses).unwrap_or(u32::MAX);
        self.tool_uses.fetch_add(calls, Ordering::Relaxed);
        self.messages
            .fetch_add(calls.saturating_add(1), Ordering::Relaxed);
    }

    /// Record one `toolDenialKind`. The oracle splits the same values two ways:
    /// `interrupted` / `cancelled` are `tool_abort`, everything else
    /// (`user-rejected`, `permission-rule`, `automode-*`) is `tool_denial`.
    pub fn note_denial(&self, kind: &str) {
        let counter = if matches!(kind, "interrupted" | "cancelled") {
            &self.aborts
        } else {
            &self.denials
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a compact boundary — the oracle's `blocking_system_in_span`.
    pub fn note_compaction(&self) {
        self.compactions.fetch_add(1, Ordering::Relaxed);
    }

    /// Read every counter once.
    #[must_use]
    pub fn snapshot(&self) -> TurnSpanCounts {
        TurnSpanCounts {
            tool_uses: self.tool_uses.load(Ordering::Relaxed),
            messages: self.messages.load(Ordering::Relaxed),
            denials: self.denials.load(Ordering::Relaxed),
            aborts: self.aborts.load(Ordering::Relaxed),
            compactions: self.compactions.load(Ordering::Relaxed),
        }
    }

    fn counters(&self) -> [&AtomicU32; 5] {
        [
            &self.tool_uses,
            &self.messages,
            &self.denials,
            &self.aborts,
            &self.compactions,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_counts_its_calls_and_the_messages_they_make() {
        let tally = TurnSpanTally::default();
        tally.note_assistant_response(2);
        let counts = tally.snapshot();
        assert_eq!(counts.tool_uses, 2);
        // the assistant message + one tool_result per call
        assert_eq!(counts.messages, 3);
    }

    /// The oracle's own split: `toolDenialKind === "interrupted" | "cancelled"`
    /// is `tool_abort`; every other kind is `tool_denial`.
    #[test]
    fn abort_kinds_are_counted_apart_from_denials() {
        let tally = TurnSpanTally::default();
        for kind in ["interrupted", "cancelled"] {
            tally.note_denial(kind);
        }
        for kind in [
            "user-rejected",
            "permission-rule",
            "automode-blocked",
            "automode-unavailable",
            "automode-parsing-error",
        ] {
            tally.note_denial(kind);
        }
        let counts = tally.snapshot();
        assert_eq!((counts.aborts, counts.denials), (2, 5));
    }

    #[test]
    fn reset_clears_every_counter() {
        let tally = TurnSpanTally::default();
        tally.note_assistant_response(3);
        tally.note_denial("user-rejected");
        tally.note_compaction();
        tally.reset();
        assert_eq!(tally.snapshot(), TurnSpanCounts::default());
    }
}
