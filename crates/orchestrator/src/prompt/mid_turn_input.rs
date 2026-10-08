//! Mid-turn input injection — the within-a-turn drain of queued user input.
//!
//! claude-code drains queued input WITHIN a running turn (query.ts mid-turn
//! injection, ~1570-1580): at each top-of-loop step it snapshots the
//! highest-priority `Next`/`Now` main-thread, non-slash commands, batches the
//! consecutive prompts (`joinPromptValues`), and injects them as a meta user
//! message so the model sees the new input on its NEXT sampling — without
//! waiting for the turn to end. This module is the orchestrator-side seam for
//! that behavior.
//!
//! ## Decoupling
//!
//! Mirrors [`crate::prompt::task_notification::TaskNotificationProvider`]: the
//! orchestrator defines the trait but the msgqueue-backed implementation lives
//! at the composition root (`engine-desktop` / `bridge-server`), so the
//! orchestrator keeps NO dependency on `msgqueue` (it is a lower layer). The
//! source drains its queue and may preserve each input's host-stamped origin
//! until `session.receive` has screened it. Queue filtering and consume/remove
//! bookkeeping stay inside the adapter where the queue lives.
//!
//! ## Abort-reason disambiguation
//!
//! A `Now`-priority enqueue cancels the active turn's `CancellationToken` (the
//! same token a user Ctrl+C/ESC fires). The token itself carries no reason, so
//! the streaming loop cannot tell a queue-driven abort from a user interrupt and
//! would mislabel the UX (inject `[Request interrupted by user]` for what was
//! really a queued message). [`CancelReason`] is a tiny shared flag set by the
//! queue adapter when it fires the token for a `Now` command; the loop reads it
//! at the cancel-check points and, for a `QueueNowCommand` abort, ends the turn
//! WITHOUT injecting the user-interrupt message (the urgent command is then run
//! by the between-turn drain). For a `UserInterrupt` (the default), behavior is
//! byte-identical to today.
//!
//! When no source / reason flag is wired (the default), every method here is a
//! strict no-op and the turn behaves exactly as before — the locked streaming
//! fixtures are unaffected.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

/// Why the active turn's [`tokio_util::sync::CancellationToken`] was fired.
///
/// Shared between the queue adapter (which sets it) and the streaming loop
/// (which reads it at the cancel-check points). Backed by an [`AtomicU8`] in
/// [`CancelReasonFlag`] so it is `Copy`, lock-free, and cheap to poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// A user pressed Ctrl+C / ESC (or any non-queue cancel). The DEFAULT —
    /// the loop injects the existing interrupt message (Ctrl+C UX unchanged).
    UserInterrupt,
    /// A `Now`-priority command was enqueued, which aborts the in-flight turn so
    /// the urgent command runs next. The loop ends the turn WITHOUT injecting
    /// the user-interrupt message (the command is the "interruption").
    QueueNowCommand,
}

impl CancelReason {
    const USER_INTERRUPT: u8 = 0;
    const QUEUE_NOW_COMMAND: u8 = 1;

    const fn to_u8(self) -> u8 {
        match self {
            Self::UserInterrupt => Self::USER_INTERRUPT,
            Self::QueueNowCommand => Self::QUEUE_NOW_COMMAND,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            Self::QUEUE_NOW_COMMAND => Self::QueueNowCommand,
            // Any other value (incl. the 0 default) is a plain user interrupt —
            // the safe-backward default so an un-set flag never mislabels.
            _ => Self::UserInterrupt,
        }
    }
}

/// A lock-free, cheaply-clonable holder for the active turn's [`CancelReason`].
///
/// The queue adapter clones this and calls [`Self::set`] right before firing the
/// turn's cancel token for a `Now` command; the streaming loop reads it via
/// [`Self::get`] at the cancel-check points. Defaults to
/// [`CancelReason::UserInterrupt`] so a turn aborted by anything OTHER than the
/// queue (Ctrl+C, ESC, a pre-cancelled token) keeps today's interrupt UX.
#[derive(Debug, Clone)]
pub struct CancelReasonFlag {
    inner: Arc<AtomicU8>,
}

impl CancelReasonFlag {
    /// A fresh flag defaulting to [`CancelReason::UserInterrupt`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicU8::new(CancelReason::UserInterrupt.to_u8())),
        }
    }

    /// Record the reason the turn is being cancelled. Call BEFORE firing the
    /// token so the loop reads the right reason when it observes cancellation.
    pub fn set(&self, reason: CancelReason) {
        self.inner.store(reason.to_u8(), Ordering::SeqCst);
    }

    /// Read the current reason. An un-set flag reads [`CancelReason::UserInterrupt`].
    #[must_use]
    pub fn get(&self) -> CancelReason {
        CancelReason::from_u8(self.inner.load(Ordering::SeqCst))
    }

    /// Reset to the default ([`CancelReason::UserInterrupt`]) at turn start so a
    /// stale `QueueNowCommand` from a prior turn cannot mislabel this one.
    pub fn reset(&self) {
        self.inner
            .store(CancelReason::UserInterrupt.to_u8(), Ordering::SeqCst);
    }
}

impl Default for CancelReasonFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// Supplies queued user input to inject WITHIN a running turn.
///
/// The streaming loop calls [`Self::take_mid_turn_input`] at each top-of-loop
/// cancel-check point. The adapter (at the composition root, backed by
/// `msgqueue`) snapshots the highest-priority `Next`/`Now` main-thread,
/// non-slash commands, joins the consecutive prompts via `join_prompt_values`,
/// `consume`s the folded commands from the queue, and returns the joined text.
/// Returns `None` when nothing batchable is queued → no injection this step.
///
/// The returned text is injected as a user message so the next sampling sees it.
/// Hosts with external arrivals override [`Self::take_mid_turn_batch`] to retain
/// each origin through the Mod receive screen before the text is joined.
#[async_trait]
pub trait MidTurnInputSource: Send + Sync {
    /// Whether this host can admit cancellable goal wake-ups.
    fn supports_goal_retries(&self) -> bool {
        false
    }
    /// Whether queued main-session work should run before a goal wake-up.
    async fn has_queued_goal_work(&self) -> bool {
        false
    }
    /// Enqueue a synthetic goal wake-up under host-owned execution and cancellation.
    async fn enqueue_goal_retry(
        &self,
        _id: String,
        _body: String,
        _cancel: tokio_util::sync::CancellationToken,
    ) {
    }

    /// Drain + return the joined text of the queued main-thread, non-slash
    /// prompts at or above `Next` priority, or `None` when there is nothing to
    /// inject. CONSUME-ONCE: drained commands are `consume`d from the queue so
    /// they are not re-run by the between-turn drain.
    async fn take_mid_turn_input(&self) -> Option<String>;

    /// Preserve individual arrivals and their host-stamped origin before they
    /// are joined for the model. Existing text-only sources use the default.
    async fn take_mid_turn_batch(&self) -> Option<Vec<MidTurnInput>> {
        self.take_mid_turn_input().await.map(|text| {
            vec![MidTurnInput {
                text,
                origin_kind: None,
            }]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidTurnInput {
    pub text: String,
    pub origin_kind: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_reason_flag_defaults_to_user_interrupt() {
        let f = CancelReasonFlag::new();
        assert_eq!(f.get(), CancelReason::UserInterrupt);
    }

    #[test]
    fn cancel_reason_flag_set_and_get_roundtrip() {
        let f = CancelReasonFlag::new();
        f.set(CancelReason::QueueNowCommand);
        assert_eq!(f.get(), CancelReason::QueueNowCommand);
        // A clone shares the same underlying atomic.
        let c = f.clone();
        assert_eq!(c.get(), CancelReason::QueueNowCommand);
        f.reset();
        assert_eq!(c.get(), CancelReason::UserInterrupt);
    }

    #[test]
    fn cancel_reason_u8_roundtrip_and_unknown_is_user_interrupt() {
        assert_eq!(
            CancelReason::from_u8(CancelReason::UserInterrupt.to_u8()),
            CancelReason::UserInterrupt
        );
        assert_eq!(
            CancelReason::from_u8(CancelReason::QueueNowCommand.to_u8()),
            CancelReason::QueueNowCommand
        );
        // An out-of-range byte falls back to UserInterrupt (safe default).
        assert_eq!(CancelReason::from_u8(99), CancelReason::UserInterrupt);
    }

    /// A mock source returning a fixed text once, then None — the shape the
    /// streaming-loop drain expects.
    struct OnceSource(std::sync::Mutex<Option<String>>);

    #[async_trait]
    impl MidTurnInputSource for OnceSource {
        async fn take_mid_turn_input(&self) -> Option<String> {
            self.0.lock().unwrap().take()
        }
    }

    #[tokio::test]
    async fn source_drains_once_then_empty() {
        let src = OnceSource(std::sync::Mutex::new(Some("queued\ninput".to_string())));
        assert_eq!(
            src.take_mid_turn_input().await.as_deref(),
            Some("queued\ninput")
        );
        assert_eq!(src.take_mid_turn_input().await, None);
    }
}
