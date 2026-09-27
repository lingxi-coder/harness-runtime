//! The end-of-turn sequence, and shared end-of-turn signal handling
//! (§4 / §5.3).
//!
//! `end_of_turn_sequence` is the part all three entries run identically: Stop
//! hooks before the token budget, then the end event. It existed in three copies
//! before, which is the drift this plan is about — a change to the order had to
//! be made three times or it silently applied to one path.
//!
//! Everything else here is shared CONSUMPTION of a signal, not the ORDER the
//! signals are consulted in. Order stays with each driver, because it is
//! observable: batched resolves the lone wakeup before the EndConversation slot,
//! streaming the other way round, and the two leave different next-turn state as
//! a result (`tests/turn_end_conversation_boundary_test.rs`).

use super::*;

impl ConversationOrchestrator {
    /// Consume a pending `EndConversation` request, surfacing the user-facing
    /// end message when one fired.
    ///
    /// Like `take_lone_wakeup_turn_end`, this CONSUMES by being called: the swap
    /// runs whether or not a request was pending, so a driver that calls it
    /// speculatively has already cleared the slot. Unlike the wakeup flag,
    /// nothing guards this call today — both drivers reach it unconditionally on
    /// the tool-round path, so the slot never survives a turn that got that far.
    ///
    /// WHERE it is called is the driver's business and is not shared: batched
    /// calls it after resolving the lone wakeup, streaming before, and that
    /// asymmetry decides whether a wakeup arming survives the turn.
    pub(crate) async fn take_end_conversation_request(&self) -> bool {
        let requested = self
            .end_conversation_slot
            .as_ref()
            .is_some_and(|slot| slot.swap(false, std::sync::atomic::Ordering::SeqCst));
        if requested {
            self.output
                .emit_text(crate::prompt::end_conversation::END_CONVERSATION_ENDED_MESSAGE)
                .await;
        }
        requested
    }
}

impl ConversationOrchestrator {
    /// The end-of-turn sequence, once, for all three entries: Stop hooks BEFORE
    /// the token budget (`query.ts:1262-1308`), then the end event.
    ///
    /// This ordering existed in three copies — one per driver — which is exactly
    /// the drift the plan is about: a change to it had to be made three times or
    /// it silently applied to one path. What each entry keeps is the NAMING of
    /// the result ([`TurnEndVerdict`]) and, above that, of the turn.
    ///
    /// `parentAborted` is read from `state.user_cancel`, which each entry filled
    /// at `TurnLoopState::new`: `run_turn` stores `None` and can never report
    /// true, the other two store their live token. Reading it here rather than
    /// taking it as an argument means there is no way for one entry to pass the
    /// wrong thing.
    ///
    /// `allow_budget_continuation` and `tool_requested_end` come from the step.
    /// Streaming has no live `stop_reason` — it is structurally in the
    /// `"end_turn"` branch — and passes the constants its own two finish helpers
    /// used before.
    pub(crate) async fn end_of_turn_sequence(
        &self,
        state: &mut TurnLoopState,
        stop_reason: &str,
        final_message_id: MessageId,
        allow_budget_continuation: bool,
        tool_requested_end: bool,
    ) -> TurnEndVerdict {
        // hooks B4: fire Stop hooks BEFORE the token-budget check
        // (order: recovery → stop-hooks → token-budget).
        if tool_requested_end {
            self.fire_tool_result_end_stop_hooks(
                stop_reason,
                state.stop_hook_active,
                token_aborted(&state.user_cancel),
            )
            .await;
        } else {
            match self
                .handle_stop_at_end(
                    stop_reason,
                    &mut state.stop_hook_active,
                    &mut state.stop_hook_blocking_count,
                    state.turn_count,
                    final_message_id,
                    token_aborted(&state.user_cancel),
                )
                .await
            {
                // `handle_stop_at_end` already emitted the end-turn here, so no
                // caller re-emits on this branch.
                StopHookFlow::Terminate(outcome) => {
                    return TurnEndVerdict::StopHookTerminated(outcome)
                }
                StopHookFlow::TerminateMaxTurns => return TurnEndVerdict::MaxTurns,
                StopHookFlow::LoopAgain => {
                    // RECOV.4: a Stop hook forced the loop to continue — reset the
                    // max_output_tokens recovery bookkeeping so the continued turn
                    // starts a fresh escalation episode (TS `query.ts:1291` sets
                    // `maxOutputTokensRecoveryCount: 0` +
                    // `maxOutputTokensOverride: undefined` on the
                    // stop-hook-blocking continuation).
                    state.recovery.reset_max_output_tokens_recovery();
                    return TurnEndVerdict::Continue;
                }
                StopHookFlow::FallThrough => {}
            }
        }
        // A3: at a natural end-of-turn, consult the token budget. If it says
        // `continue`, inject the meta nudge, reset the A1 recovery count (per
        // `query.ts:1332`), and loop again instead of ending. When budget is off
        // this is a no-op.
        //
        // Gate on `end_turn`: the batched entries carry the live `stop_reason`,
        // so a TERMINAL end (blocking_limit / prompt_too_long) must NOT trigger
        // budget continuation. Streaming is structurally in the `"end_turn"`
        // branch and passes it as such.
        if allow_budget_continuation
            && stop_reason == "end_turn"
            && self
                .maybe_continue_for_budget(
                    state.budget.as_mut(),
                    &mut state.recovery,
                    state.global_turn_tokens,
                )
                .await
        {
            return TurnEndVerdict::Continue;
        }
        let cost = self.snapshot_cost_real().await;
        self.output.emit_end_turn(stop_reason, &cost).await;
        TurnEndVerdict::EndTurn(final_message_id)
    }
}
