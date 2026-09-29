//! Batched, cancelable, and streaming conversation turn drivers.
//!
//! Three public entries — `run_turn`, `run_turn_with_cancel`,
//! `run_turn_streaming` — over two loops. The per-turn work they share lives
//! in the submodules: `prepare` (reminders, the fifteen preparation steps, the
//! prompt snapshot), `loop_state` (`TurnLoopState`, the top-of-loop guards, and
//! the two result types — `StepExit` for how an iteration ended, `TurnEndVerdict`
//! for what the end-of-turn sequence decided) and `disposition` (that sequence
//! and the end-of-turn signal consumption).
//!
//! All three loops have the same outer shape: guards, one round, a match on
//! what it returned. What stays per-entry is what actually differs, and each
//! difference is held by a test that asserts it rather than a comment that
//! describes it: the guard ORDER (`LoopGuardOrder` — the cancelable entry does
//! not drain, streaming checks cancel last), cancel handling, the NAMING of
//! every terminal (§3.2: the same verdict is an `Err` on one entry and an `Ok`
//! on another), and the file-history epilogue, which only the streaming loop
//! runs.

mod disposition;
mod loop_state;
mod prepare;
mod streaming;

use super::*;
use loop_state::{StepExit, TurnEndVerdict};
use protocol::ContentBlock;
use streaming::StreamingTurnDriver;

/// One queued prompt, retaining its own transcript identity and origin class.
#[derive(Clone, Debug, Default)]
pub struct QueuedPromptInput {
    /// Opaque identity for admission of a cancellable goal retry.
    pub goal_retry_id: Option<String>,
    /// The already-expanded text to deliver to the model.
    pub text: String,
    /// Synthetic scheduled input stays meta even beside a human prompt.
    pub is_meta: bool,
    /// Queue-supplied identity; generated when absent.
    pub message_id: Option<MessageId>,
    /// Native queue priority retained only in the JSONL host envelope.
    pub queue_priority: Option<String>,
    /// Scheduled-job identity retained only in the JSONL host envelope.
    pub scheduled_task_id: Option<String>,
    /// Fire identity; written only when a scheduled task id is present.
    pub scheduled_fire_id: Option<String>,
}

/// Drop runs on success, error, and cancellation of the driving future.
struct MainLoopActivityGuard {
    provider: Option<Arc<dyn crate::prompt::task_notification::TaskNotificationProvider>>,
    interactive: bool,
}

impl Drop for MainLoopActivityGuard {
    fn drop(&mut self) {
        if let Some(provider) = &self.provider {
            provider.update_shell_session_activity(self.interactive, false, false);
        }
    }
}

/// Mutable state shared by the phases of one streaming turn.
///
/// Keeping these counters together makes their session/turn lifetime explicit
/// and lets the streaming driver hand a single state value to its preparation,
/// pump, finalize, tool, and disposition phases.
/// The per-turn loop state all three entries share.
///
/// Named for the loop rather than the transport since PR 3: the batched and
/// streaming loops keep their own shapes, but they no longer keep their own
/// idea of what a turn's state IS. Each field is per-TURN — a state object that
/// outlives the turn that owns it turns `turn_count` into a session total and
/// freezes the output-token baseline, and
/// `tests/turn_loop_state_boundary_test.rs` fails on both.
pub(crate) struct TurnLoopState {
    recovery: RecoveryState,
    stop_hook_active: bool,
    stop_hook_blocking_count: u32,
    budget: Option<BudgetTracker>,
    global_turn_tokens: u64,
    turn_count: u32,
    malformed_tool_use_retried: bool,
    thinking_only_nudged: bool,
    last_message_id: MessageId,
    /// The turn's user-cancel token, so the stop-hook firings reached through
    /// `&ConversationOrchestrator` (which does not own one) can still report
    /// `parentAborted` on `tengu_goal_evaluated`.
    user_cancel: Option<CancellationToken>,
}

impl TurnLoopState {
    fn new(
        orch: &ConversationOrchestrator,
        last_message_id: MessageId,
        user_cancel: Option<CancellationToken>,
    ) -> Self {
        // claude-code `D = Date.now()` at the top of the query generator: the
        // duration base for the analytics that fire from its `finally`.
        *orch.compaction_runtime.query_started_at.lock().unwrap() = std::time::Instant::now();
        orch.compaction_runtime.turn_start_output_baseline.store(
            orch.compaction_runtime
                .output_token_pool
                .load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        Self {
            recovery: RecoveryState::default(),
            stop_hook_active: false,
            stop_hook_blocking_count: 0,
            budget: orch.new_budget_tracker(),
            global_turn_tokens: 0,
            turn_count: 0,
            malformed_tool_use_retried: false,
            thinking_only_nudged: false,
            last_message_id,
            user_cancel,
        }
    }
}

enum StreamingIterationDisposition {
    Continue,
    /// A natural completion may absorb input queued while the model streamed.
    Complete(MessageId),
    /// A result-level end request is terminal for this query. Pending input is
    /// left for the next turn instead of causing another model invocation.
    ForcedComplete(MessageId),
    Return(ConversationOutcome),
}

/// `p.abortController.signal.aborted` — whether the USER cancelled this turn.
///
/// Read at each stop-hook firing so `tengu_goal_evaluated` can report
/// `parentAborted`, and so a goal evaluation that produced no verdict is
/// classified `cancelled` rather than `absent`.
fn token_aborted(token: &Option<CancellationToken>) -> bool {
    token.as_ref().is_some_and(CancellationToken::is_cancelled)
}

impl ConversationOrchestrator {
    /// Read the active turn's [`crate::prompt::mid_turn_input::CancelReason`] from
    /// the wired flag, defaulting to `UserInterrupt` when no flag is wired (so an
    /// un-wired turn always takes the user-interrupt branch — today's behavior).
    fn cancel_reason_now(&self) -> crate::prompt::mid_turn_input::CancelReason {
        self.cancel_reason.get().map_or(
            crate::prompt::mid_turn_input::CancelReason::UserInterrupt,
            |f| f.get(),
        )
    }

    /// Mid-turn drain step: pull any queued main-thread, non-slash input from the
    /// wired source and inject it as a plain (non-meta) user message so the next
    /// sampling sees it — CC 2.1.207's `queued_command` guard leaves plain human
    /// input non-meta (`r!==void 0&&!Ree(r)||e.isMeta` → `{}`). A strict no-op
    /// when no source is wired (the default) or the queue
    /// is empty. Returns `true` if anything was injected (for the caller's
    /// observability — the loop continues regardless). Mirrors claude-code's
    /// `joinPromptValues` + meta-prompt injection at query.ts ~1570-1580.
    async fn drain_mid_turn_input(&self) -> bool {
        let mut injected = self.drain_peer_inbox(true).await;
        let Some(source) = self.mid_turn_input.get() else {
            return injected;
        };
        // Loop so a burst of consecutive enqueues all land before the next call.
        // The production source ([`MsgQueueMidTurnInput`]) is consume-once — it
        // REMOVES the commands it returns each call — so it self-terminates after
        // it has drained the queue. The bound below is a defensive guard against a
        // MALFUNCTIONING source impl (the trait is public; a buggy impl that fails
        // to consume could otherwise return `Some` forever and hang the turn loop):
        // we cap the per-iteration drain at a generous fixed number of batches so a
        // single drain step can never spin unboundedly.
        const MAX_DRAIN_BATCHES: usize = 1024;
        for _ in 0..MAX_DRAIN_BATCHES {
            match source.take_mid_turn_input().await {
                Some(text) => {
                    self.reset_goal_interruption();
                    let wrapped = Self::wrap_mid_turn_user_message(&text);
                    self.inject_user_message(&wrapped).await;
                    injected = true;
                }
                None => break,
            }
        }
        injected
    }

    /// Wrap joined mid-turn user input in the 2.1.206 envelope (`YAt`, binary
    /// @225654300) before injection. The `human` / `auto-continuation` / unset
    /// arm — the only source the port's `MsgQueueMidTurnInput` produces (all
    /// mid-turn input is user-typed) — prefixes `jca` ("The user sent a new
    /// message while you were working:\n") and appends the explainer. Em-dash is
    /// U+2014; "Claude Code" -> "LingXi" per the brand rebrand. (206 dropped the
    /// 201 "IMPORTANT: After completing your current task…" suffix — 0 hits in
    /// 206.) A non-user source would instead use
    /// `"[MESSAGE FROM NON-USER SOURCE - NOT USER INPUT]\n{text}"`, but the port
    /// has no such mid-turn source today.
    #[must_use]
    fn wrap_mid_turn_user_message(text: &str) -> String {
        format!(
            "The user sent a new message while you were working:\n{text}\n\nThis is how LingXi surfaces messages the user sends mid-turn \u{2014} within the running turn, often alongside the next tool result, rather than as a separate conversation turn. Address the message above as you continue this turn."
        )
    }

    /// Inject an engine META user message (`isMeta:true`) into both the live
    /// session history and the JSONL persistence stream — the recovery /
    /// continuation nudges (thinking-only, malformed-tool retry) that claude-code
    /// creates via `createUserMessage({ …, isMeta: true })`. Persisting stamps the
    /// top-level `isMeta:true` envelope flag (see `to_jsonl_message`), so these
    /// lines are skipped by title / first-prompt / fork-name / visible-count
    /// extraction, exactly as in CC 2.1.207.
    async fn inject_meta_user_message(&self, text: &str) {
        self.inject_user_text(text, true).await;
    }

    /// Inject a PLAIN (non-meta) user text message. Used for interrupt markers
    /// (`[Request interrupted by user]`) and mid-turn drained HUMAN input, which
    /// CC 2.1.207 persists WITHOUT `isMeta` — interrupt lines are built with no
    /// `isMeta` field, and queued human input is non-meta per the `queued_command`
    /// guard (`r!==void 0&&!Ree(r)||e.isMeta` → `{}` for plain human input).
    pub(super) async fn inject_user_message(&self, text: &str) {
        self.inject_user_text(text, false).await;
    }

    /// Drain accepted cross-session inbox lines into history as meta user-role
    /// `<cross-session-message>` envelopes (2.1.232 `isMeta:!0`). Policy is
    /// applied at receive; this only injects already-accepted bodies.
    pub(crate) async fn drain_peer_inbox(&self, mid_turn: bool) -> bool {
        let reminders = platform_api::live_sessions::take_accepted_peer_reminders(mid_turn);
        if reminders.is_empty() {
            return false;
        }
        for body in reminders {
            // Peer / receipt text is never user intent (2.1.232 `isMeta:!0`).
            self.inject_user_text(&body, true).await;
        }
        true
    }

    async fn inject_user_text(&self, text: &str, is_meta: bool) {
        let msg = if is_meta {
            ConversationMessage::user_meta(MessageId::new(), text.to_string())
        } else {
            ConversationMessage::user(MessageId::new(), text.to_string())
        };
        {
            let mut s = self.session.lock().await;
            s.history.push(msg.clone());
        }
        self.persist_message_to_jsonl(&msg).await;
    }

    async fn maybe_continue_for_budget(
        &self,
        budget: Option<&mut BudgetTracker>,
        recovery: &mut RecoveryState,
        global_turn_tokens: u64,
    ) -> bool {
        // No tracker → feature off / no budget → never continue (parity no-op).
        let Some(tracker) = budget else {
            return false;
        };
        // The orchestrator turn loop has no sub-agent `agentId` concept here
        // (that lives in the agent-spawn path); pass `None`, matching the main
        // query loop where `toolUseContext.agentId` is undefined for the root.
        let decision =
            check_token_budget(tracker, None, self.config.token_budget, global_turn_tokens);
        match decision {
            TokenBudgetDecision::Continue {
                nudge_message,
                continuation_count,
                pct,
                turn_tokens,
                budget,
            } => {
                tracing::info!(
                    event = "token_budget_continuation",
                    continuation_count,
                    pct,
                    turn_tokens,
                    budget,
                    "token budget continuation #{continuation_count}: {pct}% ({turn_tokens} / {budget})"
                );
                // Inject the continuation nudge as a META user message
                // (`createUserMessage({content: nudgeMessage, isMeta: true})`,
                // query.ts:1327). It persists with top-level `isMeta:true` and is
                // skipped by title / first-prompt / visible-count extraction.
                let nudge_msg = ConversationMessage::user_meta(MessageId::new(), nudge_message);
                {
                    let mut s = self.session.lock().await;
                    s.history.push(nudge_msg.clone());
                }
                self.persist_message_to_jsonl(&nudge_msg).await;
                // Reset the A1 recovery count on each budget continuation
                // (TS `query.ts:1332` `maxOutputTokensRecoveryCount: 0` +
                // `maxOutputTokensOverride: undefined`; REC.A1: a fresh recovery
                // episode may escalate again).
                recovery.reset_max_output_tokens_recovery();
                true
            }
            TokenBudgetDecision::Stop { completion_event } => {
                if let Some(ev) = completion_event {
                    if ev.diminishing_returns {
                        tracing::info!(
                            event = "token_budget_completed",
                            pct = ev.pct,
                            "token budget early stop: diminishing returns at {}%",
                            ev.pct
                        );
                    }
                    tracing::info!(
                        event = "token_budget_completed",
                        continuation_count = ev.continuation_count,
                        pct = ev.pct,
                        turn_tokens = ev.turn_tokens,
                        budget = ev.budget,
                        diminishing_returns = ev.diminishing_returns,
                        duration_ms = u64::try_from(ev.duration_ms).unwrap_or(u64::MAX),
                    );
                }
                false
            }
        }
    }

    /// B6-T1: status-change emit parity for a turn that DIED on a rate limit.
    ///
    /// claude-code's terminal catch handler `extractQuotaStatusFromError`
    /// (claudeAiLimits.ts:487) forces the limits to `status='rejected'` and
    /// runs `emitStatusChange` (ts:509-511) ALONGSIDE rendering the terminal
    /// error copy — so the TUI shows the rate-limit banner (+ the T5 overage
    /// notice) next to the assistant error message. By the time a turn driver
    /// surfaces a terminal `RateLimited`, the drive fn has already PROMOTED
    /// the staged 429 into `self.api`'s caches (via
    /// [`crate::provider_adapter::ProviderApiAdapter::promote_pending_429`]),
    /// so these emit-on-change helpers flow the rejected snapshot (+ raw
    /// windows) out as an [`platform_api::OutputEvent::RateLimit`] (+ `RawUtilization`).
    ///
    /// Gated on the rate-limited discriminant a terminal error carries BEFORE
    /// enrichment — `ApiCall(RateLimited)` (batched) or `Streaming(RateLimited)`
    /// (stream connect-phase) — so a non-429 terminal never emits. Called
    /// AFTER the drive fn returned (promotion done) and BEFORE
    /// [`Self::enrich_api_error`].
    ///
    /// B1 — DOCUMENTED DIVERGENCE (not parity): TS forces `status='rejected'`
    /// and emits even on a HEADERLESS terminal 429 (claudeAiLimits.ts:506-507,
    /// outside the headers block). The Rust `last_rate_limit` has no "bare
    /// rejected, no windows" representation, so a headerless terminal 429
    /// promotes nothing and these emit-on-change helpers are no-ops — the
    /// terminal error copy already conveys the rejection.
    async fn emit_terminal_rate_limit_if_changed<T>(&self, result: &Result<T, OrchestratorError>) {
        if let Err(
            OrchestratorError::ApiCall(LlmError::RateLimited { .. })
            | OrchestratorError::Streaming(LlmError::RateLimited { .. }),
        ) = result
        {
            self.emit_rate_limit_if_changed().await;
            self.emit_raw_utilization_if_changed().await;
        }
    }

    /// What both batched entries do once a turn-step has returned: accumulate
    /// its output tokens, then run the end-of-turn sequence — Stop hooks BEFORE
    /// the token budget (`query.ts:1262-1308`).
    ///
    /// Returns what happened rather than an outcome, exactly as
    /// [`loop_state::GuardVerdict`] does for the guards above it. The two
    /// entries name these terminals differently — `run_turn` speaks
    /// `ConversationOutcome` and ends the Stop-hook max-turns branch as
    /// `Err(MaxTurnsReached)`, `run_turn_with_cancel` speaks `TurnOutcome` and
    /// ends it as `Ok(TurnOutcome::MaxTurns)` — and §3.2 keeps that difference.
    /// Folding the mapping in here is what would erase it, so it stays at each
    /// call site, where a test can see the two disagree.
    ///
    /// `parent_cancel` is the entry's user-cancel token rather than a resolved
    /// bool, so claude-code's `parentAborted` is still read at the same two
    /// Stop-hook sites it is read at today. The non-cancelable entry has no
    /// token and passes `None`, which is why `parentAborted` can never be true
    /// there.
    async fn run_batched_round(
        &self,
        state: &mut TurnLoopState,
        step: TurnStepOutcome,
        output_tokens: u64,
    ) -> TurnEndVerdict {
        // A3: accumulate the running per-turn output tokens (TS
        // `getTurnOutputTokens()`). No-op for accounting when budget is off.
        // One site for both entries, reached exactly once per step.
        state.global_turn_tokens = state.global_turn_tokens.saturating_add(output_tokens);
        match step {
            TurnStepOutcome::Continue => TurnEndVerdict::Continue,
            TurnStepOutcome::Ended {
                final_message_id: id,
                stop_reason,
                allow_budget_continuation,
                tool_requested_end,
            } => {
                self.end_of_turn_sequence(
                    state,
                    &stop_reason,
                    id,
                    allow_budget_continuation,
                    tool_requested_end,
                )
                .await
            }
        }
    }

    /// Drive one user prompt through the turn loop until `end_turn` or
    /// `max_turns` is exhausted.
    ///
    /// Emits 3 telemetry events:
    /// - [`orch_events::CONVERSATION_STARTED`] at entry
    /// - [`orch_events::CONVERSATION_COMPLETED`] on success
    /// - [`orch_events::CONVERSATION_FAILED`] on error
    pub async fn run_turn(&self, prompt: &str) -> Result<ConversationOutcome, OrchestratorError> {
        let _turn_guard = self.turn_gate.lock().await;
        let _activity_guard = self.main_loop_activity(true);
        tracing::info!(
            event = orch_events::CONVERSATION_STARTED,
            prompt_len = prompt.len()
        );
        let result = telemetry::otel::with_turn_span("lingxi.orchestrator.turn", async {
            self.scope_api_session(!self.prompt_is_interactive(), self.try_run_turn(prompt))
                .await
        })
        .await;
        self.emit_terminal_rate_limit_if_changed(&result).await;
        let result = result.map_err(|e| self.enrich_api_error(e));
        // ConversationOutcome is #[non_exhaustive] so future variants will
        // also log as Completed when the only existing variant is EndTurn.
        match &result {
            Ok(
                ConversationOutcome::EndTurn { turn_count, .. }
                | ConversationOutcome::StopHookPrevented { turn_count, .. },
            ) => {
                tracing::info!(
                    event = orch_events::CONVERSATION_COMPLETED,
                    turn_count = *turn_count
                );
            }
            Err(err) => {
                tracing::error!(
                    event = orch_events::CONVERSATION_FAILED,
                    reason = %err
                );
            }
        }
        result
    }

    async fn try_run_turn(&self, prompt: &str) -> Result<ConversationOutcome, OrchestratorError> {
        // 0. Build the system prompt for THIS turn.
        // claude-code `nre` precedence: `--system-prompt` (override) wins; else
        // the `--agent`-adopted main-thread agent's prompt; else the default.
        let system_prompt: Option<String> = Some(self.effective_system_prompt().await);

        // A side query left unfinished when the previous user turn ended was
        // keyed to that previous prompt. Never surface it against new intent.
        self.discard_stale_prefetches().await;

        // 1. Append the user prompt to session history.
        // 2.1.266 `vSt`: a user message re-opens the idle-check-in budget that
        // `RUe`'s cap closed ("idle check-ins paused until your next message").
        // Only the deferral's idle counter is reset; the stretch itself lives on.
        self.lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear_idle_checkins();
        let user_msg = ConversationMessage::user(MessageId::new(), prompt.to_string());
        {
            let mut s = self.session.lock().await;
            s.history.push(user_msg.clone());
        }
        self.persist_message_to_jsonl(&user_msg).await;

        // hooks B4: fire UserPromptSubmit. A Block decision aborts the turn
        // BEFORE any API call (TS prompt-ingress hook). No-op when unregistered.
        if self.fire_user_prompt_submit(prompt, user_msg.id()).await {
            return Ok(ConversationOutcome::StopHookPrevented {
                turn_count: 0,
                final_message_id: user_msg.id(),
            });
        }

        // 2. Turn-by-turn driver.
        // A1: per-conversation max_output_tokens recovery bookkeeping carried
        // across turn-steps (the 3-retry limit is consecutive).
        // hooks B4: Stop-hook re-entry guard. Set true after a Stop hook blocks
        // and we loop once more; a second block then passes (no infinite loop).
        // #2 consecutive Stop-hook block counter (binary `stopHookBlockingCount`):
        // bumped per block; ends the turn via the cap once it would exceed
        // LINGXI_STOP_HOOK_BLOCK_CAP (default 8). Fresh per turn-driver run.
        // A3: token-budget continuation bookkeeping. `Some` only when the gate
        // is enabled AND a budget is set; otherwise the budget check is a
        // NO-OP and the loop stops at the first `end_turn` (parity default).
        // Turn-start output baseline (claude-code `xtr` via `UAc(e)`): snapshot the
        // cumulative pool as this turn begins, so a workflow launched this turn
        // reads `budget.spent()` = `pool - baseline` (output spent THIS turn).
        let turn_message_id = MessageId::new();
        self.begin_output_turn(turn_message_id).await?;
        // claude-code `D = Date.now()` at the top of the query generator: the
        // duration base for the analytics that fire from its `finally`.
        // Shared per-turn loop state, constructed HERE, directly after
        // `begin_output_turn`, because the constructor is what takes
        // `query_started_at` and the output-token baseline — §5.1 freezes that
        // adjacency, and `tests/turn_loop_state_boundary_test.rs` checks it.
        let mut state = TurnLoopState::new(self, turn_message_id, None);
        let final_message_id;
        loop {
            // Streaming twin already drains here (query.ts ~1570). Claude Code
            // has one main loop; the batched print path must consume mid-turn
            // input before max_turns / budget so a queued message is not dropped.
            match self
                .run_turn_loop_guards(loop_state::LoopGuardOrder::Batched, &mut state)
                .await
            {
                loop_state::GuardVerdict::Proceed => {}
                loop_state::GuardVerdict::MaxTurns => {
                    return Err(OrchestratorError::MaxTurnsReached {
                        max_turns: self.config.max_turns,
                    })
                }
                loop_state::GuardVerdict::OverBudget => {
                    return Err(OrchestratorError::MaxBudgetReached {
                        budget_nano_usd: self.config.max_budget_nano_usd.unwrap_or(0),
                    })
                }
            }

            let (step, output_tokens) = execute_one_turn_with_recovery_tracked(
                self,
                system_prompt.as_deref(),
                Some(&mut state.recovery),
            )
            .await?;
            match self
                .run_batched_round(&mut state, step, output_tokens)
                .await
            {
                TurnEndVerdict::Continue => continue,
                TurnEndVerdict::StopHookTerminated(outcome) => return Ok(outcome),
                TurnEndVerdict::MaxTurns => {
                    return Err(OrchestratorError::MaxTurnsReached {
                        max_turns: self.config.max_turns,
                    })
                }
                TurnEndVerdict::EndTurn(id) => {
                    final_message_id = id;
                    break;
                }
            }
        }

        Ok(ConversationOutcome::EndTurn {
            turn_count: state.turn_count,
            final_message_id,
        })
    }

    /// Drive one user prompt through the STREAMING turn loop. Mirrors
    /// the contract of [`Self::run_turn`] but consumes SSE events as
    /// they arrive (per-token `OutputStream::emit_text`) and dispatches
    /// `tool_use` blocks the moment their `content_block_stop` event is
    /// received.
    ///
    /// Emits 2 streaming-specific telemetry events at the boundaries:
    /// - [`orch_events::TURN_STREAMING_STARTED`] at entry.
    /// - [`orch_events::TURN_STREAMING_COMPLETED`] after success.
    ///
    /// On error, the existing [`orch_events::CONVERSATION_FAILED`] is
    /// reused (no new error event in M5-04).
    pub async fn run_turn_streaming(
        &self,
        prompt: &str,
    ) -> Result<ConversationOutcome, OrchestratorError> {
        let _turn_guard = self.turn_gate.lock().await;
        let _activity_guard = self.main_loop_activity(true);
        tracing::info!(
            event = orch_events::TURN_STREAMING_STARTED,
            prompt_len = prompt.len()
        );
        // DEFERRED-3: the plain (non-cancelable) streaming entry has no granular
        // user-interrupt token → `None` (behaviour byte-identical to before).
        let result = telemetry::otel::with_turn_span("lingxi.orchestrator.turn.streaming", async {
            self.scope_api_session(
                !self.prompt_is_interactive(),
                Box::pin(self.try_run_turn_streaming(prompt, Vec::new(), None, None, false, true)),
            )
            .await
        })
        .await;
        self.emit_terminal_rate_limit_if_changed(&result).await;
        let result = result.map_err(|e| self.enrich_api_error(e));
        match &result {
            Ok(
                ConversationOutcome::EndTurn { turn_count, .. }
                | ConversationOutcome::StopHookPrevented { turn_count, .. },
            ) => {
                tracing::info!(
                    event = orch_events::TURN_STREAMING_COMPLETED,
                    turn_count = *turn_count
                );
            }
            Err(err) => {
                tracing::error!(
                    event = orch_events::CONVERSATION_FAILED,
                    reason = %err
                );
            }
        }
        result
    }

    /// Start a model turn solely to deliver completed async-hook responses.
    ///
    /// No synthetic human prompt is added to history or JSONL. The completed
    /// hook buffer contributes the transient `async_hook_response` meta user
    /// message during request assembly, so the provider still receives a valid
    /// user boundary while the transcript remains faithful.
    pub async fn run_async_hook_rewake(&self) -> Result<TurnOutcome, OrchestratorError> {
        let _turn_guard = self.turn_gate.lock().await;
        self.run_meta_rewake_under_gate().await
    }

    /// Host-owned idle turn: callers install their normal cancellation and
    /// permission lifecycle before entering here. Recheck under the turn gate.
    pub async fn run_task_notification_rewake(
        &self,
        registry: &dyn platform_api::task_registry::TaskRegistryHandle,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let _turn_guard = self.turn_gate.lock().await;
        if cancel.is_cancelled() || !registry.has_pending_task_notifications_for(None).await {
            // The host already reserved its UI/permission lifecycle. Close it
            // even when a preceding turn consumed this completion at the gate.
            let cost = self.snapshot_cost_real().await;
            self.output.emit_end_turn("end_turn", &cost).await;
            return Ok(if cancel.is_cancelled() {
                TurnOutcome::Cancelled
            } else {
                TurnOutcome::EndTurn
            });
        }
        self.run_meta_rewake_under_gate_with_cancel(Some(cancel))
            .await
    }

    fn main_loop_activity(&self, user_interaction: bool) -> MainLoopActivityGuard {
        let provider = self.prompt_runtime.task_notifications.clone();
        let interactive = self.prompt_is_interactive();
        if let Some(provider) = &provider {
            provider.update_shell_session_activity(interactive, true, user_interaction);
        }
        MainLoopActivityGuard {
            provider,
            interactive,
        }
    }

    async fn run_meta_rewake_under_gate(&self) -> Result<TurnOutcome, OrchestratorError> {
        self.run_meta_rewake_under_gate_with_cancel(None).await
    }

    async fn run_meta_rewake_under_gate_with_cancel(
        &self,
        cancel: Option<CancellationToken>,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let _activity_guard = self.main_loop_activity(false);
        let cancel_probe = cancel.clone();
        self.output.emit_turn_started().await;
        let result = self
            .scope_api_session(
                !self.prompt_is_interactive(),
                Box::pin(self.try_run_turn_streaming("", Vec::new(), cancel, None, true, false)),
            )
            .await;
        self.emit_terminal_rate_limit_if_changed(&result).await;
        match result {
            Ok(
                ConversationOutcome::EndTurn { .. } | ConversationOutcome::StopHookPrevented { .. },
            ) => Ok(
                if cancel_probe
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                {
                    TurnOutcome::Cancelled
                } else {
                    TurnOutcome::EndTurn
                },
            ),
            Err(OrchestratorError::MaxTurnsReached { .. }) => {
                let cost = self.snapshot_cost_real().await;
                self.output.emit_end_turn("max_tokens", &cost).await;
                Ok(TurnOutcome::MaxTurns)
            }
            Err(error) => {
                let error = self.enrich_api_error(error);
                self.output
                    .emit_system_notice(&error.to_string(), true)
                    .await;
                let cost = self.snapshot_cost_real().await;
                self.output.emit_end_turn("error", &cost).await;
                Err(error)
            }
        }
    }

    async fn drive_streaming_tools(
        &self,
        exec: &mut crate::streaming_executor::StreamingToolExecutor<'_>,
        pumped: &crate::streaming_loop::PumpedTurn,
        tool_use_parent_uuids: &std::collections::HashMap<protocol::ToolUseId, String>,
        assistant_uuid: &Option<String>,
    ) -> (bool, Vec<hooks::events::PostToolBatchCall>) {
        // 5. Drive tools through the StreamingToolExecutor (faithful port of
        //    claude-code's `StreamingToolExecutor` + `query.ts:826-862`).
        //    Each tool runs the same hook + permission + registry pipeline
        //    (via `dispatch_tool_uses_tracked` per tool) under concurrency
        //    control, and EACH result is persisted as its OWN `user` message
        //    parented to the originating assistant (per-result,
        //    assistant-parented topology), in RECEIVED order — diverging from
        //    the old single-batched-user-message shape and matching the TS
        //    `sessionStorage` `sourceToolAssistantUUID → parentUuid` mapping.
        let mut prevent_continuation = false;
        let mut post_tool_batch_calls = Vec::new();
        if !pumped.tool_uses.is_empty() {
            // The executor (`exec`) was created BEFORE the stream and its
            // tools were registered + dispatched MID-STREAM by
            // `pump_stream_with_executor` (claude-code `query.ts:837-844`);
            // on the non-streaming 529-fallback path it was rebuilt above and
            // its tools added from the fallback response. Here we only DRIVE
            // it to completion + persist — `add_tool` no longer happens
            // post-stream on the normal path.
            //
            // Drive to completion, persisting each result IN RECEIVED ORDER
            // as its own user message parented to the originating assistant.
            let mut all_modifiers: Vec<tool_api::ContextModifier> = Vec::new();
            loop {
                // (no-op unless a Bash sibling errored / the turn discarded)
                // A queued tool cancelled here gets the SAME
                // `user_interrupted` synthetic `drain_one` substitutes, and
                // claude-code stamps that message `user-rejected`
                // (`createSyntheticErrorMessage`, 2.1.220 @232972524), so
                // record the kind for the persisted tool_result line.
                for (id, reason, is_mcp) in exec.apply_abort_to_pending() {
                    if reason == crate::streaming_executor::AbortReason::UserInterrupted {
                        self.record_tool_denial_kind(
                            &id,
                            if is_mcp {
                                "interrupted"
                            } else {
                                "user-rejected"
                            },
                        )
                        .await;
                    }
                    // O1: the synthetic that survives carries claude's own
                    // short `toolUseResult` literal, not the block's text.
                    self.record_tool_use_result(
                        &id,
                        crate::streaming_executor::synthetic_tool_use_result_for_tool(
                            reason, is_mcp,
                        ),
                    )
                    .await;
                }
                exec.process_queue();
                // persist whatever just completed, in order
                for drained in exec.take_newly_completed() {
                    prevent_continuation |= drained.prevent_continuation;
                    post_tool_batch_calls.extend(drained.post_tool_batch_calls);
                    // Parent this tool_result to ITS tool_use's per-block
                    // assistant line uuid (TS `sourceToolAssistantUUID`),
                    // falling back to the turn's last assistant block uuid if
                    // the id isn't in the map (defensive — e.g. an append that
                    // failed and was skipped above).
                    let parent_uuid = match &drained.block {
                        ContentBlock::ToolResult { tool_use_id, .. } => tool_use_parent_uuids
                            .get(tool_use_id)
                            .cloned()
                            .or_else(|| assistant_uuid.clone()),
                        _ => assistant_uuid.clone(),
                    };
                    // Release this tool's SDK frame HERE — `take_newly_completed`
                    // yields in received order, and `drained.block` is the
                    // post-substitution content, so a cancelled tool reports
                    // its synthetic rather than the real outcome the executor
                    // discarded. A queued-then-cancelled tool never dispatched
                    // and so has no buffered frame; it gets one from here,
                    // where previously it got none at all.
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = &drained.block
                    {
                        self.release_tool_frame(tool_use_id, &drained.tool, content, *is_error)
                            .await;
                    }
                    // O3: keep this result's tool id so its hook
                    // `attachment` lines can be flushed immediately after
                    // its tool_result — claude's stream order.
                    let drained_tool_use_id = match &drained.block {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    };
                    let user_msg = ConversationMessage::User {
                        id: MessageId::new(),
                        content: vec![drained.block],
                        is_meta: false,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
                    };
                    {
                        let mut s = self.session.lock().await;
                        s.history.push(user_msg.clone());
                    }
                    // `bash_output_audience_note` (NEW in 2.1.238, gate `kpm`
                    // @294267076, emission @294300924): a Bash result whose
                    // stdout is longer than the few lines the user's terminal
                    // shows gets a one-line note telling the model the user
                    // did NOT see it. The oracle pushes it into the message
                    // STREAM right after the `tool_result` line, so it lands
                    // in history + JSONL like any other attachment message.
                    // Gated on the model capability
                    // `bash_output_audience_note` / the
                    // `CLAUDE_CODE_BASH_OUTPUT_AUDIENCE_NOTE` env var; the
                    // port has no capability table ⇒ DEFAULT OFF ⇒ strict
                    // no-op, so the locked streaming fixtures are unaffected.
                    //
                    // Computed BEFORE the persist below: the JSONL writer
                    // CONSUMES the recorded `toolUseResult` (it moves into
                    // the line's `toolUseResult` field), and the gate needs
                    // that payload's `stdout`.
                    let audience_note = match &drained_tool_use_id {
                        Some(id) => self.bash_output_audience_note_message(id).await,
                        None => None,
                    };
                    self.persist_message_to_jsonl_with_parent(&user_msg, parent_uuid)
                        .await;
                    if let Some(note) = audience_note {
                        {
                            let mut s = self.session.lock().await;
                            s.history.push(note.clone());
                        }
                        self.persist_message_to_jsonl(&note).await;
                    }
                    if let Some(id) = &drained_tool_use_id {
                        self.flush_hook_attachments(id).await;
                    }
                    // SKILLEXEC.3 (streaming): replay tool-injected
                    // `new_messages` (the Skill tool's expanded prompt) right
                    // after the tool_result, recording each injected message's
                    // id → originating tool_use_id into the in-memory
                    // `injected_message_sources` side-table (faithful port of
                    // TS `sourceToolUseID`). These chain normally (NOT a parent
                    // override) — matching the batched path — so the JSONL
                    // bytes stay byte-identical. Empty for every non-skill tool
                    // → strict no-op.
                    for (m, tool_use_id) in drained.injected {
                        let is_ephemeral_rendering = m.is_meta();
                        {
                            let mut s = self.session.lock().await;
                            s.history.push(m.clone());
                            s.injected_message_sources.insert(m.id(), tool_use_id);
                        }
                        // O3: an `is_meta` injected message is the
                        // EPHEMERAL rendering of the attachment flushed
                        // above — persisting it would duplicate the record.
                        if is_ephemeral_rendering {
                            continue;
                        }
                        self.persist_message_to_jsonl(&m).await;
                    }
                    all_modifiers.extend(drained.modifiers);
                }
                // Guaranteed-progress shape (mirrors the test-only
                // `run_to_completion`): an empty in-flight set after
                // `process_queue` means no Queued tool remains startable
                // (process_queue starts any runnable one) and nothing is
                // executing — so every tool is done + drained above. Break
                // here, else drain one future below. Never spins: each
                // iteration either breaks or `.await`s a completion.
                if exec.inflight_is_empty() {
                    break;
                }
                exec.drain_one().await;
            }
            // SKILLEXEC.3 (model scope, streaming twin): fold this turn's
            // `context_modifier`s and switch `session.model` if a skill
            // declared a `model:` override. Applied AFTER all results +
            // injected messages, so there is no race on `session.model`.
            // Empty for every non-`model:` tool → strict no-op.
            crate::turn_loop::apply_model_context_modifiers(self, all_modifiers).await;
        }
        // Drive finished: stop holding frames.
        self.set_tool_frame_buffering(false).await;
        // Concurrent safe tools can finish out of order. PostToolBatch is
        // defined from the assistant's original `toolUseBlocks.map(...)`, so
        // restore received order before firing the single batch event.
        let mut ordered_batch_calls = Vec::with_capacity(post_tool_batch_calls.len());
        for tool_use in &pumped.tool_uses {
            if let Some(index) = post_tool_batch_calls
                .iter()
                .position(|call| call.tool_use_id == tool_use.id)
            {
                ordered_batch_calls.push(post_tool_batch_calls.remove(index));
            }
        }
        // Defensive preservation for a future synthetic call whose id is not
        // represented in `pumped.tool_uses`.
        ordered_batch_calls.extend(post_tool_batch_calls);
        (prevent_continuation, ordered_batch_calls)
    }

    /// A natural streaming end, through the shared end-of-turn sequence.
    ///
    /// Streaming has no live `stop_reason` here — it is structurally in the
    /// `"end_turn"` branch — so it passes that and the budget gate open. What
    /// stays here is the naming: the same verdict that raises `MaxTurnsReached`
    /// on this path returns `Ok(TurnOutcome::MaxTurns)` on the cancelable
    /// batched one (§3.2).
    async fn finish_natural_streaming_end(
        &self,
        loop_state: &mut TurnLoopState,
        assistant_id: MessageId,
    ) -> Result<StreamingIterationDisposition, OrchestratorError> {
        match self
            .end_of_turn_sequence(loop_state, "end_turn", assistant_id, true, false)
            .await
        {
            TurnEndVerdict::Continue => Ok(StreamingIterationDisposition::Continue),
            TurnEndVerdict::StopHookTerminated(outcome) => {
                Ok(StreamingIterationDisposition::Return(outcome))
            }
            TurnEndVerdict::MaxTurns => Err(OrchestratorError::MaxTurnsReached {
                max_turns: self.config.max_turns,
            }),
            // `Complete`, not `ForcedComplete`: a natural end still gets the
            // loop's one final drain before it commits.
            TurnEndVerdict::EndTurn(id) => Ok(StreamingIterationDisposition::Complete(id)),
        }
    }

    /// A tool-requested streaming end, through the same shared sequence.
    ///
    /// `tool_requested_end` sends it down the `fire_tool_result_end_stop_hooks`
    /// branch — no Stop-hook verdict to act on — and closes the budget gate, so
    /// only `EndTurn` is reachable in practice; the other arms are mapped
    /// honestly rather than declared impossible.
    async fn finish_tool_requested_streaming_end(
        &self,
        loop_state: &mut TurnLoopState,
        assistant_id: MessageId,
    ) -> Result<StreamingIterationDisposition, OrchestratorError> {
        match self
            .end_of_turn_sequence(loop_state, "end_turn", assistant_id, false, true)
            .await
        {
            TurnEndVerdict::Continue => Ok(StreamingIterationDisposition::Continue),
            TurnEndVerdict::StopHookTerminated(outcome) => {
                Ok(StreamingIterationDisposition::Return(outcome))
            }
            TurnEndVerdict::MaxTurns => Err(OrchestratorError::MaxTurnsReached {
                max_turns: self.config.max_turns,
            }),
            // `ForcedComplete`: a tool asked the turn to end, so it does NOT get
            // the loop's final drain — that is the whole difference between this
            // helper and the natural one.
            TurnEndVerdict::EndTurn(id) => Ok(StreamingIterationDisposition::ForcedComplete(id)),
        }
    }

    async fn decide_streaming_disposition(
        &self,
        loop_state: &mut TurnLoopState,
        pumped: &crate::streaming_loop::PumpedTurn,
        assistant_id: MessageId,
        tool_prevent_continuation: bool,
        post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
        pre_batch_mcp_tool_count: usize,
    ) -> Result<StreamingIterationDisposition, OrchestratorError> {
        // #78 nudge guard `!Pt(ce)` (streaming twin): suppress the
        // thinking-only nudge during a StructuredOutput exchange. Computed
        // before the match (a match guard cannot `.await` the session lock);
        // reused by all three streaming nudge sites (end_turn / stop_sequence
        // / missing). The current assistant response is already in `history`.
        let prior_structured_output = {
            let s = self.session.lock().await;
            crate::turn_loop::prior_assistant_used_structured_output(&s.history)
        };

        // `/loop` fold span (streaming twin of the count in `turn_loop`): this
        // response's tool calls, and the messages it adds. Counted here because
        // this is the one point every streamed response passes through.
        self.turn_span
            .note_assistant_response(pumped.tool_uses.len());

        // The oracle guard checks the immediately preceding transition,
        // rather than whether any earlier attempt in this turn was malformed.
        if pumped.stop_reason.as_deref() != Some("tool_use") || !pumped.tool_uses.is_empty() {
            loop_state.malformed_tool_use_retried = false;
        }

        // 6. Decide loop disposition.
        match pumped.stop_reason.as_deref() {
            // #1 needsFollowUp gate (claude-code `query.ts:554-558`/`832-835`/
            // `1062`): continuation is keyed on tool-block PRESENCE, NOT the raw
            // `stop_reason` string (the ref notes `stop_reason == "tool_use"` is
            // "unreliable"). Any response that dispatched tool_use blocks runs
            // the tools (already driven above) AND continues — feeding the
            // tool_results back — regardless of whether the stop_reason was
            // `tool_use`, `end_turn`, `stop_sequence`, or a truncated
            // `max_tokens` that still carried a complete tool block. Fires only
            // when tools were dispatched; a withheld `max_output_tokens`
            // response carries NO tool_uses and falls through to the recovery/
            // terminal arms below. Subsumes the former
            // `Some("tool_use") if !pumped.tool_uses.is_empty()` arm.
            _ if !pumped.tool_uses.is_empty() => {
                let turn_end = self
                    .take_pending_tool_result_turn_ends(
                        &pumped
                            .tool_uses
                            .iter()
                            .map(|tool_use| tool_use.id.clone())
                            .collect::<Vec<_>>(),
                    )
                    .await;
                if tool_prevent_continuation {
                    let cost = self.snapshot_cost_real().await;
                    self.output.emit_end_turn("hook_stopped", &cost).await;
                    return Ok(StreamingIterationDisposition::Return(
                        ConversationOutcome::EndTurn {
                            turn_count: loop_state.turn_count,
                            final_message_id: assistant_id,
                        },
                    ));
                }
                if let Some(turn_end) = turn_end {
                    crate::turn_loop::emit_tool_result_ended_turn_telemetry(self, turn_end).await;
                    let batch_messages =
                        crate::turn_loop::run_post_tool_batch_hooks_after_turn_end(
                            self,
                            post_tool_batch_calls,
                        )
                        .await;
                    crate::turn_loop::append_tool_injected_messages(self, batch_messages).await;
                    return self
                        .finish_tool_requested_streaming_end(loop_state, assistant_id)
                        .await;
                }
                let (batch_prevent, batch_messages) =
                    crate::turn_loop::run_post_tool_batch_hooks(self, post_tool_batch_calls).await;
                crate::turn_loop::append_tool_injected_messages(self, batch_messages).await;
                if batch_prevent {
                    let cost = self.snapshot_cost_real().await;
                    self.output.emit_end_turn("hook_stopped", &cost).await;
                    return Ok(StreamingIterationDisposition::Return(
                        ConversationOutcome::EndTurn {
                            turn_count: loop_state.turn_count,
                            final_message_id: assistant_id,
                        },
                    ));
                }
                crate::turn_loop::emit_tools_refreshed_mid_turn_telemetry(
                    self,
                    pre_batch_mcp_tool_count,
                )
                .await;
                // EndConversation (2.1.206, streaming twin): a 2nd
                // consecutive EndConversation call raised the shared
                // end-request slot during tool dispatch above. Consume it;
                // if raised, surface the end message and terminate instead
                // of continuing. Default-OFF (no slot wired) → strict no-op
                // → byte-identical to before.
                // Consumed BEFORE the lone-wakeup check below, and the arm
                // returns on a hit — so a wakeup arming survives an
                // EndConversation turn here, where on the batched path it does
                // not. Deliberate; see the shared helper's note.
                if self.take_end_conversation_request().await {
                    return Ok(StreamingIterationDisposition::Return(
                        ConversationOutcome::EndTurn {
                            turn_count: loop_state.turn_count,
                            final_message_id: assistant_id,
                        },
                    ));
                }
                // LONE `ScheduleWakeup` ENDS THE TURN (streaming twin). Same
                // arm, same order, and the same shared flag as `turn_loop`'s —
                // the streaming loop is the one the desktop bridge actually
                // takes, which is where `/loop` runs at all.
                if crate::turn_loop::take_lone_wakeup_turn_end(
                    self,
                    pumped
                        .tool_uses
                        .iter()
                        .map(|tool_use| tool_use.name.as_str()),
                )
                .await
                {
                    crate::turn_loop::emit_loop_dynamic_wakeup_ends_turn_telemetry(self).await;
                    let cost = self.snapshot_cost_real().await;
                    self.output.emit_end_turn("end_turn", &cost).await;
                    return Ok(StreamingIterationDisposition::Complete(assistant_id));
                }
                Ok(StreamingIterationDisposition::Continue)
            }
            Some("end_turn") => {
                // #78 thinking-only nudge (claude-code `bin/claude.exe`
                // offset ~202946760): an `end_turn` response with no visible
                // text gets ONE nudge to produce user-visible output. This
                // fires BEFORE the Stop hooks (binary order: malformed →
                // thinking-only → stop-hooks → budget), so a thinking-only
                // turn re-prompts the model without first running Stop hooks.
                // The `a !== "compact" && !GRe(a)` source guard is satisfied
                // unconditionally here (compact subturns run in a separate
                // code path — `CompactionOrchestrator` — never this loop), and
                // `!isApiErrorMessage` holds because API errors are caught as
                // `Err(..)` upstream of this match. Once nudged, a still-empty
                // continuation falls through to the normal end.
                if !loop_state.thinking_only_nudged
                    && !pumped_has_visible_text(&pumped.assistant_blocks)
                    && !prior_structured_output
                {
                    self.discard_retry_attempt(assistant_id).await;
                    self.inject_meta_user_message(THINKING_ONLY_NUDGE).await;
                    loop_state.thinking_only_nudged = true;
                    return Ok(StreamingIterationDisposition::Continue);
                }
                self.finish_natural_streaming_end(loop_state, assistant_id)
                    .await
            }
            // #77 malformed-tool-use retry (claude-code `bin/claude.exe`
            // offset ~202945837): `stop_reason == "tool_use"` but the
            // assistant produced ZERO tool_use blocks (a malformed /
            // leaked-invoke response). On the FIRST such failure, inject the
            // byte-exact meta retry nudge, reset the max-output-tokens
            // recovery bookkeeping (TS resets `maxOutputTokensRecoveryCount:
            // 0` + `hasAttemptedReactiveCompact: false`), arm the guard, and
            // loop. On the SECOND (`malformed_tool_use_retried` already set),
            // surface the non-meta terminal message and end the turn. The
            // `!isApiErrorMessage` guard holds (API errors are caught
            // upstream as `Err(..)`). cc 2.1.263 unconditionally removes
            // the malformed attempt and injects the clean-retry nudge (`ZZe`).
            Some("tool_use") => {
                if loop_state.malformed_tool_use_retried {
                    // Second failure → terminal NON-meta message, complete.
                    // Binary `ql(...)`→`mcc({isApiErrorMessage:!0})`: an
                    // ASSISTANT api-error message (`role:"assistant",
                    // stop_reason:"stop_sequence", stop_details:null`) appended
                    // after the malformed assistant response (two assistants in
                    // a row, matching the binary). Shape mirrors
                    // `surface_model_error`. (Was a USER message.)
                    self.output.emit_text(MALFORMED_TOOL_USE_RETRY_FAILED).await;
                    let failed_msg = ConversationMessage::Assistant {
                        id: MessageId::new(),
                        content: vec![ContentBlock::Text {
                            text: MALFORMED_TOOL_USE_RETRY_FAILED.to_string(),
                        }],
                        stop_reason: Some("stop_sequence".to_string()),
                    };
                    {
                        let mut s = self.session.lock().await;
                        s.history.push(failed_msg.clone());
                    }
                    self.persist_api_error_message_to_jsonl(
                        &failed_msg,
                        ApiErrorEnvelope::default(),
                    )
                    .await;
                    let cost = self.snapshot_cost_real().await;
                    self.output.emit_end_turn("end_turn", &cost).await;
                    return Ok(StreamingIterationDisposition::Complete(failed_msg.id()));
                }
                self.discard_retry_attempt(assistant_id).await;
                self.inject_meta_user_message(MALFORMED_TOOL_USE_RETRY_NUDGE)
                    .await;
                // TS resets the recovery counters on the retry transition so
                // the continued turn starts a fresh max-output-tokens
                // escalation episode.
                loop_state.recovery.reset_max_output_tokens_recovery();
                loop_state.malformed_tool_use_retried = true;
                Ok(StreamingIterationDisposition::Continue)
            }
            // A1: intercept `max_tokens` BEFORE the generic terminal arm.
            // While recovery is not exhausted, inject the byte-exact meta
            // nudge user message, increment the counter, and Continue
            // (TS `query.ts:1223-1252`). On exhaustion, fall through to the
            // generic terminal below (end with stop_reason `max_tokens`).
            Some("max_tokens")
                if loop_state.recovery.max_output_tokens_recovery_count
                    < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT =>
            {
                // The nudge is a META user message carrying the byte-exact
                // string — CC 2.1.207 builds it via `createUserMessage({…,
                // isMeta:!0})`, so it persists with top-level `isMeta:true`.
                let nudge_msg = ConversationMessage::user_meta(
                    MessageId::new(),
                    MAX_OUTPUT_TOKENS_RECOVERY_NUDGE.to_string(),
                );
                {
                    let mut s = self.session.lock().await;
                    s.history.push(nudge_msg.clone());
                }
                self.persist_message_to_jsonl(&nudge_msg).await;
                loop_state.recovery.max_output_tokens_recovery_count = loop_state
                    .recovery
                    .max_output_tokens_recovery_count
                    .saturating_add(1);
                loop_state.recovery.max_output_tokens_override = None;
                Ok(StreamingIterationDisposition::Continue)
            }
            // #78 thinking-only nudge for `stop_sequence` (claude-code
            // groups `end_turn` and `stop_sequence` under one guard). A
            // `stop_sequence` response with no visible text gets the same
            // once-per-turn nudge before terminating. Intercepted ahead of
            // the generic terminal arm; once nudged it falls through.
            Some("stop_sequence")
                if !loop_state.thinking_only_nudged
                    && !pumped_has_visible_text(&pumped.assistant_blocks)
                    && !prior_structured_output =>
            {
                self.discard_retry_attempt(assistant_id).await;
                self.inject_meta_user_message(THINKING_ONLY_NUDGE).await;
                loop_state.thinking_only_nudged = true;
                Ok(StreamingIterationDisposition::Continue)
            }
            // Finding #80 (streaming twin): a `refusal` response swaps to the
            // configured `refusalFallbackModel` ONCE per session and retries.
            // Intercepted ahead of the generic terminal arm; when no fallback
            // is configured (or the latch is already set) it falls through to
            // the terminal `Some(other)` arm below, byte-identical to before.
            Some("refusal") if self.maybe_swap_to_refusal_fallback().await => {
                Ok(StreamingIterationDisposition::Continue)
            }
            Some(other) => {
                // max_tokens (recovery exhausted) / stop_sequence (visible
                // text or already nudged) / pause_turn / refusal (no fallback
                // configured / already latched) — terminate the loop with the
                // value as-is, mirroring claude-code's behavior (claude.ts:2269).
                //
                // First surface the byte-locked user-visible `API Error: …`
                // assistant message claude-code emits for the terminal
                // stop_reasons it reports as errors (`claude.ts:2266`
                // max_tokens [recovery exhausted], `:2279`
                // model_context_window_exceeded). A strict no-op for every
                // other terminal (stop_sequence / pause_turn /
                // refusal-without-fallback), so those end byte-identically to
                // before. Mirrors `surface_prompt_too_long` (persist a new
                // assistant message carrying the error text + the originating
                // stop_reason, then emit it).
                // Build the byte-locked `API Error: …` via the shared
                // [`crate::turn_loop::terminal_api_error_text`] (the batched
                // twin uses the SAME builder, so both paths surface identical
                // text). `None` for stop_sequence / pause_turn / refusal-
                // without-fallback's other terminals → no message, end as-is.
                let api_error: Option<String> = {
                    let model = self.session.lock().await.model.clone();
                    let request_id = self.api.last_request_id();
                    crate::turn_loop::terminal_api_error_text(
                        &model,
                        self.prompt_is_interactive(),
                        other,
                        request_id.as_deref(),
                        pumped.stop_details.as_ref(),
                    )
                };
                let surfaced_id = if let Some(text) = api_error {
                    let err_msg = ConversationMessage::Assistant {
                        id: MessageId::new(),
                        content: vec![ContentBlock::Text { text: text.clone() }],
                        stop_reason: Some(other.to_string()),
                    };
                    self.session.lock().await.history.push(err_msg.clone());
                    let envelope = match other {
                        "max_tokens" | "model_context_window_exceeded" => ApiErrorEnvelope {
                            error: Some("max_output_tokens"),
                            ..ApiErrorEnvelope::default()
                        },
                        "refusal" => ApiErrorEnvelope {
                            error: Some("invalid_request"),
                            inner_stop_reason: Some("refusal"),
                            ..ApiErrorEnvelope::default()
                        },
                        _ => ApiErrorEnvelope::default(),
                    };
                    self.persist_api_error_message_to_jsonl(&err_msg, envelope)
                        .await;
                    self.output.emit_text(&text).await;
                    Some(err_msg.id())
                } else {
                    None
                };
                let cost = self.snapshot_cost_real().await;
                self.output.emit_end_turn(other, &cost).await;
                Ok(StreamingIterationDisposition::Complete(
                    surfaced_id.unwrap_or(assistant_id),
                ))
            }
            None => {
                // Stream ended without a stop_reason — treat as
                // end_turn (rare; claude.ts uses the same fallback). The
                // token-budget check applies here too (A3).
                // #78: a missing stop_reason is treated as `end_turn`
                // (claude-code `stop_reason ?? <default>`), so the
                // thinking-only nudge applies here on the same terms and,
                // like the `end_turn` arm, fires BEFORE the Stop hooks.
                if !loop_state.thinking_only_nudged
                    && !pumped_has_visible_text(&pumped.assistant_blocks)
                    && !prior_structured_output
                {
                    self.discard_retry_attempt(assistant_id).await;
                    self.inject_meta_user_message(THINKING_ONLY_NUDGE).await;
                    loop_state.thinking_only_nudged = true;
                    return Ok(StreamingIterationDisposition::Continue);
                }
                self.finish_natural_streaming_end(loop_state, assistant_id)
                    .await
            }
        }
    }

    /// Internal streaming turn driver (no telemetry — wrapped by
    /// `run_turn_streaming`).
    #[allow(clippy::too_many_lines)]
    async fn try_run_turn_streaming(
        &self,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        user_cancel: Option<CancellationToken>,
        message_id: Option<MessageId>,
        transient_rewake: bool,
        in_human_turn: bool,
    ) -> Result<ConversationOutcome, OrchestratorError> {
        self.try_run_turn_streaming_inputs(
            prompt,
            images,
            user_cancel,
            message_id,
            transient_rewake,
            in_human_turn,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_run_turn_streaming_inputs(
        &self,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        user_cancel: Option<CancellationToken>,
        message_id: Option<MessageId>,
        transient_rewake: bool,
        in_human_turn: bool,
        queued_inputs: Option<Vec<QueuedPromptInput>>,
    ) -> Result<ConversationOutcome, OrchestratorError> {
        StreamingTurnDriver {
            orch: self,
            prompt,
            images,
            user_cancel,
            message_id,
            transient_rewake,
            in_human_turn,
            queued_inputs,
        }
        .run()
        .await
    }

    // Race cancellation only before execution starts. Dropping a running turn
    // would bypass its tool-result persistence and usage settlement.
    async fn lock_turn_unless_cancelled(
        &self,
        cancel: &CancellationToken,
    ) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            guard = self.turn_gate.lock() => Some(guard),
        }
    }

    /// Drive one user prompt through a REPL turn until `end_turn`,
    /// `max_turns`, or the given `cancel` token fires.
    ///
    /// Unlike [`Self::run_turn`] this method:
    /// - returns [`TurnOutcome`] so the REPL can distinguish natural end /
    ///   max-turns / cancellation.
    /// - takes a [`CancellationToken`] that fires on Ctrl+C (SIGINT); the
    ///   orchestrator checks it before each API round-trip.
    /// - does NOT close the session — the session accumulates messages across
    ///   REPL turns; the REPL persists via the JSONL writer when it exits.
    ///
    /// Telemetry: emits `CONVERSATION_STARTED` at entry; delegates to the
    /// same turn-loop body as `run_turn` (via `try_run_turn_cancelable`).
    pub async fn run_turn_with_cancel(
        &self,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let Some(_turn_guard) = self.lock_turn_unless_cancelled(&cancel).await else {
            return Ok(TurnOutcome::Cancelled);
        };
        let _activity_guard = self.main_loop_activity(true);
        tracing::info!(
            event = orch_events::CONVERSATION_STARTED,
            prompt_len = prompt.len()
        );
        let result =
            telemetry::otel::with_turn_span("lingxi.orchestrator.turn.cancelable", async {
                self.scope_api_session(
                    !self.prompt_is_interactive(),
                    self.try_run_turn_cancelable(prompt, cancel),
                )
                .await
            })
            .await;
        self.emit_terminal_rate_limit_if_changed(&result).await;
        result.map_err(|e| self.enrich_api_error(e))
    }

    /// Internal implementation of the REPL turn loop with cancellation.
    async fn try_run_turn_cancelable(
        &self,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        // 0. Build the system prompt (same as non-cancelable path).
        // claude-code `nre` precedence: `--system-prompt` (override) wins; else the
        // `--agent` main-thread agent's prompt; else the default (`build_system_prompt`).
        let system_prompt: Option<String> = Some(self.effective_system_prompt().await);

        // A side query left unfinished when the previous user turn ended was
        // keyed to that previous prompt. Never surface it against new intent.
        self.discard_stale_prefetches().await;

        // 1. Append the user prompt to session history.
        // 2.1.266 `vSt`: a user message re-opens the idle-check-in budget that
        // `RUe`'s cap closed ("idle check-ins paused until your next message").
        // Only the deferral's idle counter is reset; the stretch itself lives on.
        self.lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear_idle_checkins();
        let user_msg = ConversationMessage::user(MessageId::new(), prompt.to_string());
        {
            let mut s = self.session.lock().await;
            s.history.push(user_msg.clone());
        }
        self.persist_message_to_jsonl(&user_msg).await;

        // hooks B4: UserPromptSubmit (cancelable REPL twin). A Block aborts the
        // turn before any API call. No-op when unregistered.
        if self.fire_user_prompt_submit(prompt, user_msg.id()).await {
            return Ok(TurnOutcome::EndTurn);
        }

        // 2. Turn-by-turn loop — check cancel before each API call.
        //
        // #2 (main-loop parity): the cancelable driver is recovery- AND
        // budget-aware, identical to the non-cancelable [`Self::run_turn`]
        // batched loop, except each API round-trip is raced against `cancel`.
        // The legacy no-recovery shim ([`execute_one_turn`]) is no longer used
        // here: a `max_tokens` stop_reason now drives the A1 multi-turn recovery
        // nudge (and exhaustion-ends) exactly as the main batched path does,
        // rather than legacy-continuing without the nudge.
        // hooks B4: Stop-hook re-entry guard (cancelable twin).
        // #2 consecutive Stop-hook block counter (binary `stopHookBlockingCount`):
        // bumped per block; ends the turn via the cap once it would exceed
        // LINGXI_STOP_HOOK_BLOCK_CAP (default 8). Fresh per turn-driver run.
        // A3: token-budget continuation bookkeeping (no-op unless gated + set).
        // Turn-start output baseline (claude-code `xtr` via `UAc(e)`): snapshot
        // the cumulative pool as this turn begins, so a workflow launched this
        // turn reads `budget.spent()` = output spent THIS turn.
        let turn_message_id = MessageId::new();
        self.begin_output_turn(turn_message_id).await?;
        // claude-code `D = Date.now()` at the top of the query generator: the
        // duration base for the analytics that fire from its `finally`.
        // Shared per-turn loop state; the token rides along so the stop-hook
        // firings reached through `&ConversationOrchestrator` can report
        // `parentAborted`.
        let mut state = TurnLoopState::new(self, turn_message_id, Some(cancel.clone()));
        loop {
            if cancel.is_cancelled() {
                // claude-code `query.ts:1046-1050`: inject the non-tool-use
                // interrupt message on a loop-top pre-cancel (ESC fired before
                // we even called the model this iteration). NOW-ABORT
                // disambiguation: skip the message when the cancel was a
                // `Now`-command (the urgent command runs next); default behavior
                // (no reason flag wired) is byte-identical to before.
                if self.cancel_reason_now()
                    != crate::prompt::mid_turn_input::CancelReason::QueueNowCommand
                {
                    self.inject_user_message(INTERRUPT_MESSAGE).await;
                }
                return Ok(TurnOutcome::Cancelled);
            }
            match self
                .run_turn_loop_guards(loop_state::LoopGuardOrder::BatchedCancelable, &mut state)
                .await
            {
                loop_state::GuardVerdict::Proceed => {}
                loop_state::GuardVerdict::MaxTurns => return Ok(TurnOutcome::MaxTurns),
                loop_state::GuardVerdict::OverBudget => {
                    return Err(OrchestratorError::MaxBudgetReached {
                        budget_nano_usd: self.config.max_budget_nano_usd.unwrap_or(0),
                    })
                }
            }

            // Race the recovery-aware API turn-step against the cancellation
            // token. The `_tracked` variant returns this step's output-token
            // count for the A3 budget accumulation, mirroring `run_turn`.
            let (step, output_tokens) = tokio::select! {
                r = execute_one_turn_with_recovery_tracked(
                    self,
                    system_prompt.as_deref(),
                    Some(&mut state.recovery),
                ) => r?,
                () = cancel.cancelled() => {
                    // claude-code `query.ts:1046-1050`: inject the non-tool-use
                    // interrupt message when the cancel fires mid-API-call
                    // (model was in-flight, no tool_use blocks produced yet).
                    // NOW-ABORT disambiguation: skip the message for a
                    // `Now`-command abort (default behavior unchanged).
                    if self.cancel_reason_now()
                        != crate::prompt::mid_turn_input::CancelReason::QueueNowCommand
                    {
                        self.inject_user_message(INTERRUPT_MESSAGE).await;
                    }
                    return Ok(TurnOutcome::Cancelled);
                }
            };
            match self
                .run_batched_round(&mut state, step, output_tokens)
                .await
            {
                TurnEndVerdict::Continue => continue,
                // `TurnOutcome` does not distinguish StopHookPrevented from
                // EndTurn, so a Stop-hook termination ends the REPL turn as
                // EndTurn — the outcome the twin returns verbatim is dropped here
                // on purpose.
                TurnEndVerdict::StopHookTerminated(_) => return Ok(TurnOutcome::EndTurn),
                // Binary blocking-branch max-turns end — mirror this fn's own
                // top-of-loop guard, which returns `TurnOutcome::MaxTurns` where
                // the twin returns `Err(MaxTurnsReached)`.
                TurnEndVerdict::MaxTurns => return Ok(TurnOutcome::MaxTurns),
                // This path has no epilogue and no final-message-id to carry, so
                // it returns straight out instead of leaving the loop.
                TurnEndVerdict::EndTurn(_) => return Ok(TurnOutcome::EndTurn),
            }
        }
    }

    /// Streaming twin of [`Self::run_turn_with_cancel`] (M6-03).
    ///
    /// DEFERRED-3: the `cancel` token is a GRANULAR user-interrupt (ESC / new
    /// message), threaded INTO [`Self::try_run_turn_streaming`] rather than raced
    /// against it. When it fires mid-tools the `StreamingToolExecutor` rejects
    /// in-flight/queued Cancel-behavior tools with the bare REJECT_MESSAGE,
    /// PERSISTS those `tool_result`s, and the turn ends gracefully:
    /// - natural completion, token never fired → `TurnOutcome::EndTurn`.
    /// - token fired (the turn finished gracefully with the interrupted results
    ///   recorded in history) → `TurnOutcome::Cancelled`.
    /// - a pre-cancelled token → `TurnOutcome::Cancelled` immediately (no API call).
    /// - `OrchestratorError::MaxTurnsReached` → `TurnOutcome::MaxTurns`.
    /// - any other API/streaming error → propagated as `Err`.
    ///
    /// This is the entry point the M6 TUI calls. M5-13 stdio REPL keeps
    /// using `run_turn_with_cancel` (batched) until M6 makes streaming
    /// the default.
    pub async fn run_turn_streaming_with_cancel(
        &self,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.run_turn_streaming_with_cancel_images_and_message_id(prompt, &[], cancel, None)
            .await
    }

    /// As [`Self::run_turn_streaming_with_cancel`], but carrying pasted image
    /// file paths that are loaded + base64-encoded into `ContentBlock::Image`
    /// blocks on the outgoing user message (TUI paste→image). A failed image
    /// read aborts the turn with `Err` before any API call.
    pub async fn run_turn_streaming_with_cancel_images(
        &self,
        prompt: &str,
        image_paths: &[std::path::PathBuf],
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.run_turn_streaming_with_cancel_images_and_message_id(prompt, image_paths, cancel, None)
            .await
    }

    /// As [`Self::run_turn_streaming_with_cancel_images`], but allows the
    /// caller to pin the persisted user-message UUID.
    pub async fn run_turn_streaming_with_cancel_images_and_message_id(
        &self,
        prompt: &str,
        image_paths: &[std::path::PathBuf],
        cancel: CancellationToken,
        message_id: Option<MessageId>,
    ) -> Result<TurnOutcome, OrchestratorError> {
        if cancel.is_cancelled() {
            return Ok(TurnOutcome::Cancelled);
        }
        // Decode the pasted PATHS into canonical sources first, then hand off to
        // the already-decoded entry below — so the path-based and bridge (inline
        // base64) flows share ONE cancel race + ONE turn core. A failed image read
        // aborts the turn with `Err` before any API call (unchanged).
        let images = Self::load_images(image_paths)?;
        self.run_turn_streaming_with_cancel_image_sources_and_message_id(
            prompt, images, cancel, message_id,
        )
        .await
    }

    /// As [`Self::run_turn_streaming_with_cancel_images`], but taking
    /// ALREADY-DECODED [`protocol::ImageSource`]s instead of file paths.
    ///
    /// This is the entry the desktop bridge adapter drives: pasted/attached images
    /// arrive over the wire as inline base64 (`ImageRefDto`) and are converted
    /// straight to [`protocol::ImageSource::Base64`] with NO temp-file round-trip.
    /// The path-based entry above decodes its paths via [`Self::load_images`] then
    /// delegates here, so both paths share this cancel race and the single
    /// [`Self::try_run_turn_streaming`] core — which appends the images to the
    /// outgoing user message via [`ConversationMessage::user_with_images`]. With an
    /// empty `images` vector this is byte-identical to the text-only streaming path.
    pub async fn run_turn_streaming_with_cancel_image_sources(
        &self,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.run_turn_streaming_with_cancel_image_sources_and_message_id(
            prompt, images, cancel, None,
        )
        .await
    }

    /// As [`Self::run_turn_streaming_with_cancel_image_sources`], but allows the
    /// caller to pin the persisted user-message UUID.
    pub async fn run_turn_streaming_with_cancel_image_sources_and_message_id(
        &self,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        cancel: CancellationToken,
        message_id: Option<MessageId>,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.run_turn_streaming_with_origin(prompt, images, cancel, message_id, true)
            .await
    }

    /// Queue adapters preserve whether a prompt batch contains genuine user input.
    pub async fn run_turn_streaming_with_origin(
        &self,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        cancel: CancellationToken,
        message_id: Option<MessageId>,
        in_human_turn: bool,
    ) -> Result<TurnOutcome, OrchestratorError> {
        if in_human_turn {
            self.reset_goal_interruption();
        }
        let Some(turn_guard) = self.lock_turn_unless_cancelled(&cancel).await else {
            return Ok(TurnOutcome::Cancelled);
        };
        self.run_turn_streaming_with_origin_locked(
            &turn_guard,
            prompt,
            images,
            cancel,
            message_id,
            in_human_turn,
        )
        .await
    }

    /// Deliver a queued batch as separate transcript messages in one model turn.
    /// Per-entry metadata is owned by this call, never stored as a session override.
    pub async fn run_queued_prompt_batch(
        &self,
        inputs: Vec<QueuedPromptInput>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        if inputs.is_empty() {
            return Ok(TurnOutcome::EndTurn);
        }
        if inputs.iter().any(|input| !input.is_meta) {
            self.reset_goal_interruption();
        }
        let Some(turn_guard) = self.lock_turn_unless_cancelled(&cancel).await else {
            return Ok(TurnOutcome::Cancelled);
        };
        let primary = inputs.iter().position(|input| !input.is_meta).unwrap_or(0);
        let prompt = inputs[primary].text.clone();
        let message_id = inputs[primary].message_id;
        let in_human_turn = inputs.iter().any(|input| !input.is_meta);
        self.run_turn_streaming_inputs_locked(
            &turn_guard,
            &prompt,
            Vec::new(),
            cancel,
            message_id,
            in_human_turn,
            Some(inputs),
        )
        .await
    }

    /// The caller retains the same turn gate through any target validation and binding.
    pub(super) async fn run_turn_streaming_with_origin_locked(
        &self,
        _turn_guard: &tokio::sync::MutexGuard<'_, ()>,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        cancel: CancellationToken,
        message_id: Option<MessageId>,
        in_human_turn: bool,
    ) -> Result<TurnOutcome, OrchestratorError> {
        self.run_turn_streaming_inputs_locked(
            _turn_guard,
            prompt,
            images,
            cancel,
            message_id,
            in_human_turn,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn_streaming_inputs_locked(
        &self,
        _turn_guard: &tokio::sync::MutexGuard<'_, ()>,
        prompt: &str,
        images: Vec<protocol::ImageSource>,
        cancel: CancellationToken,
        message_id: Option<MessageId>,
        in_human_turn: bool,
        queued_inputs: Option<Vec<QueuedPromptInput>>,
    ) -> Result<TurnOutcome, OrchestratorError> {
        if in_human_turn {
            self.reset_goal_interruption();
        }
        let mut queued_inputs = queued_inputs;
        if let Some(inputs) = &mut queued_inputs {
            let mut admitted = Vec::with_capacity(inputs.len());
            for input in inputs.drain(..) {
                if let Some(id) = input.goal_retry_id.as_deref() {
                    if !self.admit_goal_retry(id).await {
                        continue;
                    }
                }
                admitted.push(input);
            }
            *inputs = admitted;
            if inputs.is_empty() {
                self.output
                    .emit_end_turn("end_turn", &self.snapshot_cost_real().await)
                    .await;
                return Ok(TurnOutcome::EndTurn);
            }
        }
        let admitted_prompt = queued_inputs
            .as_ref()
            .and_then(|inputs| {
                inputs
                    .iter()
                    .find(|i| !i.is_meta)
                    .or_else(|| inputs.first())
            })
            .map(|i| i.text.clone());
        let prompt = admitted_prompt.as_deref().unwrap_or(prompt);
        let _activity_guard = self.main_loop_activity(in_human_turn);
        tracing::info!(
            event = orch_events::TURN_STREAMING_STARTED,
            prompt_len = prompt.len()
        );
        if cancel.is_cancelled() {
            self.abort_startup_responses_websocket_prewarm();
            if let Err(err) = self.api.close_responses_websocket_session().await {
                tracing::warn!(
                    error = %err,
                    "failed to close responses websocket session after pre-cancelled streaming turn"
                );
            }
            return Ok(TurnOutcome::Cancelled);
        }
        // DEFERRED-3: GRANULAR user-ESC interrupt — do NOT race-drop the turn.
        // Previously this `select!`'d `try_run_turn_streaming` against
        // `cancel.cancelled()` and on cancel DROPPED the whole turn future, so
        // in-flight tools vanished and no result reached the model. Now the token
        // is threaded INTO the turn core: the `StreamingToolExecutor` substitutes
        // the bare REJECT_MESSAGE for in-flight/queued Cancel-behavior tools,
        // PERSISTS those `tool_result`s (model-visible), and the streaming loop
        // FINISHES gracefully — mirroring claude-code's `StreamingToolExecutor`
        // user_interrupted path where the interrupted results become part of the
        // transcript. We then map the outcome to `Cancelled` for the TUI when the
        // token fired (so it still renders "interrupted"), else `EndTurn`. A turn
        // running only Block-behavior tools (or no tools) runs to its natural end
        // and is still reported `Cancelled` here — faithful: claude-code only
        // aborts Cancel-behavior tools; Block tools / the stream finish.
        let r = telemetry::otel::with_turn_span(
            "lingxi.orchestrator.turn.streaming.cancelable",
            async {
                self.scope_api_session(
                    !self.prompt_is_interactive(),
                    Box::pin(self.try_run_turn_streaming_inputs(
                        prompt,
                        images,
                        Some(cancel.clone()),
                        message_id,
                        false,
                        in_human_turn,
                        queued_inputs,
                    )),
                )
                .await
            },
        )
        .await;
        if cancel.is_cancelled() {
            self.reset_goal_interruption();
        }
        match r {
            Ok(
                ConversationOutcome::EndTurn { turn_count, .. }
                | ConversationOutcome::StopHookPrevented { turn_count, .. },
            ) => {
                tracing::info!(event = orch_events::TURN_STREAMING_COMPLETED, turn_count);
                if cancel.is_cancelled() {
                    self.reset_goal_interruption();
                    self.abort_startup_responses_websocket_prewarm();
                    if let Err(err) = self.api.close_responses_websocket_session().await {
                        tracing::warn!(
                            error = %err,
                            "failed to close responses websocket session after cancelled streaming turn"
                        );
                    }
                    Ok(TurnOutcome::Cancelled)
                } else {
                    Ok(TurnOutcome::EndTurn)
                }
            }
            Err(OrchestratorError::MaxTurnsReached { .. }) => Ok(TurnOutcome::MaxTurns),
            Err(OrchestratorError::VisionDelegationCancelled) if cancel.is_cancelled() => {
                Ok(TurnOutcome::Cancelled)
            }
            Err(e) => {
                // B6-T1: status-change emit parity — fire the emit-on-change
                // helpers for a terminal rate-limited error (the drive fn
                // already promoted the staged 429), BEFORE enrichment. Same
                // discriminant + B1 divergence as
                // `emit_terminal_rate_limit_if_changed`.
                if matches!(
                    e,
                    OrchestratorError::ApiCall(LlmError::RateLimited { .. })
                        | OrchestratorError::Streaming(LlmError::RateLimited { .. })
                ) {
                    self.emit_rate_limit_if_changed().await;
                    self.emit_raw_utilization_if_changed().await;
                }
                Err(self.enrich_api_error(e))
            }
        }
    }

    /// Load + base64-encode each pasted image path into an [`ImageSource`].
    fn load_images(
        image_paths: &[std::path::PathBuf],
    ) -> Result<Vec<protocol::ImageSource>, OrchestratorError> {
        image_paths
            .iter()
            .map(|p| crate::image_input::load_image_source(p))
            .collect()
    }
}

// Streaming recovery conversion and visibility helpers.
/// Convert an `HistoryResponse` (from the non-streaming fallback call) into the
/// same [`crate::streaming_loop::PumpedTurn`] shape the streaming loop uses,
/// so the remainder of the streaming turn handler works unchanged.
///
/// Mirrors the batched path's `translate_response_blocks` call: content blocks
/// are translated to `protocol::ContentBlock`; `ToolCall` blocks additionally
/// populate the `tool_uses` vec so the concurrent dispatch runs exactly as in a
/// real stream.
/// Whether the turn's assistant blocks contain any user-visible text, mirroring
/// claude-code's thinking-only guard predicate (`bin/claude.exe` offset
/// ~202946760):
/// `ie.some(msg => msg.content.some(b => b.type === "text" && b.text.trim().length > 0))`.
///
/// A `false` return = a thinking-only (or otherwise text-empty) response. Only
/// [`protocol::ContentBlock::Text`] blocks with a non-whitespace body count;
/// `Thinking`, `ToolUse`, etc. are not "visible output" for this gate. (Tool
/// uses live in `PumpedTurn::tool_uses`, not `assistant_blocks`, and this gate
/// only fires on `end_turn`/`stop_sequence` where no `tool_use` is present.)
pub(super) fn pumped_has_visible_text(blocks: &[protocol::ContentBlock]) -> bool {
    use protocol::ContentBlock;
    blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if !text.trim().is_empty()))
}

pub(super) fn llm_response_to_pumped_turn(
    resp: &HistoryResponse,
) -> crate::streaming_loop::PumpedTurn {
    use crate::streaming_loop::{ObservedToolUse, PumpedTurn};
    use crate::turn_loop::translate_response_blocks;
    use protocol::ContentBlock;

    let output_tokens = resp
        .usage
        .counts()
        .output_tokens
        .saturating_sub(resp.usage.counts().reasoning_tokens);
    let stop_reason = resp.stop_reason.clone();
    // BILLING: carry the full usage so the streaming turn loop records it
    // in CostTracker. The non-streaming fallback issues a real messages_create
    // call (seeded) whose response includes the authoritative usage.
    let usage = Some(resp.usage.clone());

    // Translate HistoryResponse content → protocol::ContentBlock (same as batched path).
    // Then split into (assistant_blocks, tool_uses): text/thinking go into
    // assistant_blocks; ToolUse blocks go into tool_uses for the concurrent dispatch.
    // The streaming loop step 4 re-assembles them into the assistant message by
    // appending ToolUse blocks from tool_uses — so we must NOT include ToolUse in
    // assistant_blocks (or they appear twice in the assistant message).
    let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_uses: Vec<ObservedToolUse> = Vec::new();

    for blk in translate_response_blocks(&resp.content) {
        match blk {
            ContentBlock::ToolUse {
                id,
                ref name,
                ref input,
                ref provider_id,
            } => {
                tool_uses.push(ObservedToolUse {
                    id,
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                });
                // Not pushed to assistant_blocks — step 4 appends ToolUse from tool_uses.
            }
            other => {
                assistant_blocks.push(other);
            }
        }
    }

    PumpedTurn {
        assistant_blocks,
        tool_uses,
        stop_reason,
        output_tokens,
        usage,
        cost_quote: resp.cost.clone(),
        // Non-streaming fallback: carry the response's refusal stop_details so
        // the terminal refusal arm gets the cyber/bio variant.
        stop_details: resp.stop_details.clone(),
    }
}

/// Mirror TS `isEnvTruthy` (`utils/envUtils.ts:32`): a value is truthy ONLY
/// when, lowercased and trimmed, it is one of the whitelist members
/// `"1"`, `"true"`, `"yes"`, `"on"`. Absent, empty, and every other value
/// (including `"no"`, `"off"`, `"2"`, `"enabled"`, …) are falsy.
///
/// Locked against the TS helper used at `claude.ts:2470`:
/// `isEnvTruthy(process.env.LINGXI_DISABLE_NONSTREAMING_FALLBACK)`.
pub(super) fn is_env_truthy(val: Option<&str>) -> bool {
    match val {
        None => false,
        Some(v) => matches!(v.to_lowercase().trim(), "1" | "true" | "yes" | "on"),
    }
}

pub(super) fn parse_generated_session_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let candidate = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|body| body.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let object = serde_json::from_str::<serde_json::Value>(candidate)
        .ok()
        .or_else(|| {
            let start = candidate.find('{')?;
            let end = candidate.rfind('}')?;
            serde_json::from_str(&candidate[start..=end]).ok()
        })?;
    object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

// NOTE: `AnthropicProviderAdapter` and `AnthropicProviderStreamingAdapter`
// were removed in Task 5 — they drove `api_client::AnthropicProvider` directly.
// The live path is now `ProviderApiAdapter` (provider_adapter.rs), retargeted
// in Task 6 to drive `llm_runtime::ModelRuntime`. (3b deletes api-client.)
