//! Top-of-loop guards, and the per-entry order they run in (§3.5).
//!
//! The three entries do not agree, and the plan keeps all three rather than
//! picking one:
//!
//! ```text
//! Batched            drain → max_turns → budget → increment
//! BatchedCancelable  cancel → max_turns → budget → increment   (NO drain)
//! Streaming          drain → max_turns → budget → increment → cancel
//! ```
//!
//! What is shared here is the drain/limit/increment sequence. CANCEL is not:
//! its position differs (first on one path, last on another, absent on the
//! third) and so does its handling — the cancelable entry injects an interrupt
//! and returns, streaming emits `aborted_streaming` and breaks so its epilogue
//! still runs. Folding those together would erase §3.2 rows that
//! `tests/turn_epilogue_boundary_test.rs` and `tests/turn_cancel_mapping_test.rs`
//! pin. Each caller keeps its own cancel handling, at its own position.

use super::*;

/// Which entry's guard order to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LoopGuardOrder {
    /// `run_turn`: drains pending input before the limits, so a queued message
    /// is not dropped by one.
    Batched,
    /// `run_turn_with_cancel`: no drain at all. A turn the user is cancelling
    /// must not pull a queued message in on its way out.
    BatchedCancelable,
    /// `run_turn_streaming`: drains like `Batched`; its cancel guard runs after
    /// the increment, which the caller does itself.
    Streaming,
}

impl LoopGuardOrder {
    /// Whether this entry drains pending input at the top of its loop.
    const fn drains(self) -> bool {
        match self {
            Self::Batched | Self::Streaming => true,
            Self::BatchedCancelable => false,
        }
    }
}

/// What the shared guards decided.
///
/// Deliberately not an outcome: the three entries map these to different public
/// types — batched returns `Ok(TurnOutcome::MaxTurns)`, streaming returns
/// `Err(OrchestratorError::MaxTurnsReached)` — and §3.2 keeps that difference.
pub(crate) enum GuardVerdict {
    /// Run the model step.
    Proceed,
    /// `max_turns` reached before this step.
    MaxTurns,
    /// The configured budget is spent.
    OverBudget,
}

impl ConversationOrchestrator {
    /// Run the drain/limit/increment guards in `order`, returning what they
    /// decided.
    ///
    /// The turn counter is incremented ONLY on `Proceed`, matching all three
    /// entries today: a turn stopped by a limit does not count the step it
    /// never ran.
    pub(crate) async fn run_turn_loop_guards(
        &self,
        order: LoopGuardOrder,
        state: &mut TurnLoopState,
    ) -> GuardVerdict {
        if order.drains() {
            // Before the limits on purpose, so a message that arrived during the
            // previous step is not dropped by `max_turns` or the budget.
            self.drain_mid_turn_input().await;
        }
        if self.config.max_turns != 0 && state.turn_count >= self.config.max_turns {
            return GuardVerdict::MaxTurns;
        }
        if self.over_budget().await {
            return GuardVerdict::OverBudget;
        }
        state.turn_count = state.turn_count.saturating_add(1);
        GuardVerdict::Proceed
    }
}

/// How one iteration of a turn loop ended (§5.4).
///
/// The loop body is not a function today because its exits are three different
/// control-flow statements — `continue`, `break` (with a message id the epilogue
/// needs), and `return` — and only a value can carry all three across a function
/// boundary. Naming them is what lets the body move out of the loop.
///
/// The distinction that matters is the last two. `FinishThroughEpilogue` leaves
/// the loop and RUNS the turn epilogue (§3.6: the streaming-only file-history
/// snapshot); `ReturnDirect` leaves the whole driver and SKIPS it. §3.6 gives
/// exactly one disposition — `Return` — the second behavior, and
/// `tests/turn_epilogue_boundary_test.rs` pins it. Collapsing the two into one
/// "the step is over" variant is the obvious simplification and the one that
/// silently hands every terminal the epilogue.
pub(crate) enum StepExit {
    /// Run another iteration.
    Continue,
    /// End the loop with this id as the turn's final message, then run the
    /// epilogue.
    FinishThroughEpilogue(MessageId),
    /// Return this outcome from the driver without running the epilogue.
    ReturnDirect(ConversationOutcome),
}

/// What the shared end-of-turn sequence decided.
///
/// A different layer from [`StepExit`]: this is what
/// `ConversationOrchestrator::end_of_turn_sequence` concluded about ONE ending,
/// while `StepExit` is what the loop then did about it. All three entries reach
/// this verdict through the same code and then name it themselves — `run_turn`
/// raises `Err(MaxTurnsReached)` for `MaxTurns` where `run_turn_with_cancel`
/// returns `Ok(TurnOutcome::MaxTurns)`, and streaming turns `EndTurn` into a
/// `Complete` that still gets the loop's final drain where a tool-requested end
/// gets `ForcedComplete` instead. §3.2 keeps those apart, so this reports the
/// EVENT and leaves the naming at each call site, exactly as [`GuardVerdict`]
/// does for the guards.
pub(crate) enum TurnEndVerdict {
    /// Run another iteration — a Stop hook asking to keep working, or the token
    /// budget granting more.
    Continue,
    /// A Stop hook ended the turn. `handle_stop_at_end` already emitted the
    /// end-turn event; the outcome it built rides along for the entries that can
    /// express it.
    StopHookTerminated(ConversationOutcome),
    /// The Stop-hook blocking branch hit `max_turns`.
    MaxTurns,
    /// Natural end of turn, end event already emitted, with the id of the final
    /// message for the entries that report one.
    EndTurn(MessageId),
}
