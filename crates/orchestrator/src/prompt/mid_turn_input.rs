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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ImageSource, MessageId};

/// The existing driver's admission boundary, before queue input is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidTurnInputPoint {
    LoopStart,
    AfterTools,
}

/// Why the active turn's [`tokio_util::sync::CancellationToken`] was fired.
///
/// Shared between the queue adapter (which sets it) and the streaming loop
/// (which reads it at the cancel-check points). Backed by an [`AtomicU64`] in
/// [`CancelReasonFlag`] so it is `Copy`, lock-free, and cheap to poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// A user pressed Ctrl+C / ESC (or another non-queue, non-shutdown cancel).
    /// The default: inject the existing unflagged interruption row.
    UserInterrupt,
    /// The host is shutting down the service. The loop still injects the
    /// Native shutdown marker, tagged `interruptedByShutdown: true`.
    ServiceShutdown,
    /// A `Now`-priority command was enqueued, which aborts the in-flight turn so
    /// the urgent command runs next. The loop ends the turn WITHOUT injecting
    /// the user-interrupt message (the command is the "interruption").
    QueueNowCommand,
    /// The host is quiescing a turn for later recovery. This is not a user
    /// interruption and must not add an interruption row to durable history.
    LifecyclePause,
}

impl CancelReason {
    const USER_INTERRUPT: u8 = 0;
    const QUEUE_NOW_COMMAND: u8 = 1;
    const SERVICE_SHUTDOWN: u8 = 2;
    const LIFECYCLE_PAUSE: u8 = 3;

    const fn to_u8(self) -> u8 {
        match self {
            Self::UserInterrupt => Self::USER_INTERRUPT,
            Self::QueueNowCommand => Self::QUEUE_NOW_COMMAND,
            Self::ServiceShutdown => Self::SERVICE_SHUTDOWN,
            Self::LifecyclePause => Self::LIFECYCLE_PAUSE,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            Self::QUEUE_NOW_COMMAND => Self::QueueNowCommand,
            Self::SERVICE_SHUTDOWN => Self::ServiceShutdown,
            Self::LIFECYCLE_PAUSE => Self::LifecyclePause,
            // Any other value (including the un-set sentinel) is a plain user interrupt —
            // the safe-backward default so an un-set flag never mislabels.
            _ => Self::UserInterrupt,
        }
    }

    pub(crate) const fn records_user_interruption(self) -> bool {
        matches!(self, Self::UserInterrupt | Self::ServiceShutdown)
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
    inner: Arc<AtomicU64>,
}

/// A reason setter bound to one turn generation. A delayed callback from an
/// older owner cannot change the reason of a newly reserved turn.
#[derive(Debug, Clone)]
pub struct CancelReasonLease {
    inner: Arc<AtomicU64>,
    generation: u64,
}

impl CancelReasonLease {
    /// Record the first reason only if this lease's turn is still current.
    pub fn set(&self, reason: CancelReason) {
        let _ = self.inner.compare_exchange(
            self.generation | CancelReasonFlag::UNSET,
            self.generation | u64::from(reason.to_u8()),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

impl CancelReasonFlag {
    const REASON_BITS: u32 = 3;
    const REASON_MASK: u64 = (1 << Self::REASON_BITS) - 1;
    const UNSET: u64 = 4;

    /// A fresh flag defaulting to [`CancelReason::UserInterrupt`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicU64::new(Self::UNSET)),
        }
    }

    /// Record the first reason the turn is being cancelled. Call BEFORE firing
    /// the token so a racing later cancellation source cannot relabel the cause.
    pub fn set(&self, reason: CancelReason) {
        let generation = self.inner.load(Ordering::SeqCst) & !Self::REASON_MASK;
        CancelReasonLease {
            inner: self.inner.clone(),
            generation,
        }
        .set(reason);
    }

    /// Read the current reason. An un-set flag reads [`CancelReason::UserInterrupt`].
    #[must_use]
    pub fn get(&self) -> CancelReason {
        CancelReason::from_u8((self.inner.load(Ordering::SeqCst) & Self::REASON_MASK) as u8)
    }

    /// Reset to the default ([`CancelReason::UserInterrupt`]) at turn start so a
    /// stale `QueueNowCommand` from a prior turn cannot mislabel this one.
    pub fn reset(&self) {
        let _ = self.reset_and_bind();
    }

    /// Start a fresh turn generation and return a setter scoped to it.
    #[must_use]
    pub fn reset_and_bind(&self) -> CancelReasonLease {
        let previous = self
            .inner
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                (value & !Self::REASON_MASK)
                    .checked_add(1 << Self::REASON_BITS)
                    .map(|generation| generation | Self::UNSET)
            })
            .expect("cancel reason generation exhausted");
        CancelReasonLease {
            inner: self.inner.clone(),
            generation: (previous & !Self::REASON_MASK) + (1 << Self::REASON_BITS),
        }
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
    /// Hosts can restrict their input to a particular native consumption point.
    /// Desktop and mobile queues retain their existing drain behavior.
    fn admits_at(&self, point: MidTurnInputPoint) -> bool {
        point != MidTurnInputPoint::AfterTools
    }

    /// Called after post-tool input has entered canonical session history.
    /// Hosts retire delivery identities and publish consumption acknowledgments here.
    async fn input_consumed(
        &self,
        _inputs: &[MidTurnInput],
    ) -> Result<(), MidTurnConsumptionError> {
        Ok(())
    }
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
        _body: Utf16JsonProjection,
        _cancel: tokio_util::sync::CancellationToken,
    ) {
    }

    /// Drain + return the joined text of the queued main-thread, non-slash
    /// prompts at or above `Next` priority, or `None` when there is nothing to
    /// inject. CONSUME-ONCE: drained commands are `consume`d from the queue so
    /// they are not re-run by the between-turn drain.
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection>;

    /// Preserve individual arrivals and their host-stamped origin before they
    /// are joined for the model. Existing text-only sources use the default.
    /// A source error is propagated before the driver admits model input.
    async fn take_mid_turn_batch(&self) -> Result<Option<Vec<MidTurnInput>>, String> {
        Ok(self.take_mid_turn_input().await.map(|text| {
            vec![MidTurnInput {
                text,
                images: Vec::new(),
                origin_kind: None,
                projected_content: None,
                source_message_uuid: None,
                queue_delivery: None,
                pending_prompt_identity: None,
            }]
        }))
    }
}

/// A stopped owner is distinct from a failure to persist a consumed input.
#[derive(Debug, thiserror::Error)]
pub enum MidTurnConsumptionError {
    #[error("the turn stopped before the input was acknowledged")]
    Interrupted,
    #[error("{0}")]
    Failure(String),
}

impl From<String> for MidTurnConsumptionError {
    fn from(message: String) -> Self {
        Self::Failure(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidTurnInput {
    pub text: Utf16JsonProjection,
    /// Ordered image sources that accompanied this Mobile prompt.
    pub images: Vec<ImageSource>,
    pub origin_kind: Option<&'static str>,
    /// Original content, including exact JavaScript strings and media blocks.
    pub projected_content: Option<Utf16JsonProjection>,
    /// Delivery identity from the source. Generated transcript IDs are not substitutes.
    pub source_message_uuid: Option<Utf16JsonProjection>,
    pub queue_delivery: Option<MidTurnInputDelivery>,
    /// Present on the first member of a durable Mobile coalesced batch. The
    /// orchestrator uses it for append-once JSONL/history publication.
    pub pending_prompt_identity: Option<PendingMidTurnPromptIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMidTurnPromptIdentity {
    pub session_id: String,
    /// First member, which owns the durable envelope/ack marker.
    pub owner_turn_id: u64,
    pub message_id: MessageId,
    pub prompt_id: String,
    /// Explicit UI correlators for this persisted aggregate row.
    pub transcript_row_token: Option<String>,
    /// Ordered Native source members; the aggregate history row uses the last
    /// member identity while this list preserves every accepted source.
    pub member_turn_ids: Vec<u64>,
}

/// Facts recorded when a host accepts an input delivery, before tool folding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidTurnInputDelivery {
    pub delivery_id: String,
    pub timestamp: String,
    pub reference_version: String,
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
    fn first_cancel_reason_wins_until_the_next_turn_reset() {
        let flag = CancelReasonFlag::new();
        flag.set(CancelReason::QueueNowCommand);
        flag.set(CancelReason::UserInterrupt);
        assert_eq!(flag.get(), CancelReason::QueueNowCommand);

        flag.reset();
        flag.set(CancelReason::ServiceShutdown);
        flag.set(CancelReason::UserInterrupt);
        assert_eq!(flag.get(), CancelReason::ServiceShutdown);

        flag.reset();
        flag.set(CancelReason::LifecyclePause);
        flag.set(CancelReason::UserInterrupt);
        assert_eq!(flag.get(), CancelReason::LifecyclePause);

        flag.reset();
        flag.set(CancelReason::UserInterrupt);
        flag.set(CancelReason::LifecyclePause);
        assert_eq!(flag.get(), CancelReason::UserInterrupt);
    }

    #[test]
    fn a_stale_turn_lease_cannot_set_the_new_turn_reason() {
        let flag = CancelReasonFlag::new();
        let old = flag.reset_and_bind();
        let current = flag.reset_and_bind();
        old.set(CancelReason::QueueNowCommand);
        assert_eq!(flag.get(), CancelReason::UserInterrupt);
        current.set(CancelReason::LifecyclePause);
        old.set(CancelReason::UserInterrupt);
        assert_eq!(flag.get(), CancelReason::LifecyclePause);
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
        assert_eq!(
            CancelReason::from_u8(CancelReason::ServiceShutdown.to_u8()),
            CancelReason::ServiceShutdown
        );
        assert_eq!(
            CancelReason::from_u8(CancelReason::LifecyclePause.to_u8()),
            CancelReason::LifecyclePause
        );
        // An out-of-range byte falls back to UserInterrupt (safe default).
        assert_eq!(CancelReason::from_u8(99), CancelReason::UserInterrupt);
    }

    /// A mock source returning a fixed text once, then None — the shape the
    /// streaming-loop drain expects.
    struct OnceSource(std::sync::Mutex<Option<String>>);

    #[async_trait]
    impl MidTurnInputSource for OnceSource {
        async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
            self.0.lock().unwrap().take().map(|text| {
                Utf16JsonProjection::root_string(text.clone(), text.encode_utf16().collect())
                    .unwrap()
            })
        }
    }

    #[tokio::test]
    async fn source_drains_once_then_empty() {
        let src = OnceSource(std::sync::Mutex::new(Some("queued\ninput".to_string())));
        assert_eq!(
            src.take_mid_turn_input()
                .await
                .and_then(|text| text.string_at("").map(str::to_owned)),
            Some("queued\ninput".to_string())
        );
        assert_eq!(src.take_mid_turn_input().await, None);
    }
}
