//! `BudgetEnforcerHandle` — narrow trait abstracting `BudgetEnforcer` so
//! `AgentTool` in `lingxi-tools` can consult the parent's budget without
//! taking a cyclic dep on `lingxi-cost`.
//!
//! Concrete impl lives in `lingxi-cost`. Tests inject a scriptable mock.
//!
//! The `BudgetError::Exceeded { current_nano_usd }` shape is what `AgentTool`
//! uses to format the M3-05 byte-locked denial string
//! (`"Budget exceeded ($X.YZ); stopped."`).

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

/// Opaque settlement token, which may also hold session budget capacity.
/// `0` is the legacy/default no-op sentinel. Concrete implementations may
/// return a non-zero token for an unlimited or zero-sized hold so a later
/// commit can still account its realized spend exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BudgetReservationId(u64);

impl BudgetReservationId {
    /// No hold and no settlement. `commit` / `release` are no-ops.
    pub const NOOP: Self = Self(0);

    /// True when this id carries neither capacity nor settlement ownership.
    #[must_use]
    pub const fn is_noop(self) -> bool {
        self.0 == 0
    }

    /// Construct a non-zero id. The cost crate allocates these.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw value for persistence / debug.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Failure modes for [`BudgetEnforcerHandle::check_and_charge`].
#[derive(Debug, Clone, Error)]
pub enum BudgetError {
    /// Budget exceeded — the caller should format the M3-05 denial string
    /// from `current_nano_usd`.
    #[error("BudgetEnforcer: exceeded at {current_nano_usd} nano-USD")]
    Exceeded {
        /// Cumulative cost at the moment of the check, in nano-USD.
        current_nano_usd: u64,
    },
    /// Any other internal failure.
    #[error("BudgetEnforcer: internal error: {0}")]
    Internal(String),
}

/// Owned settlement receipt returned after a production budget implementation
/// transfers responsibility for a known provider charge. Fusion can disarm
/// its reservation lease as soon as it receives this receipt; a later durable
/// acknowledgement or error must not re-arm or release the token.
#[async_trait]
pub trait BudgetSettlementReceipt: Send {
    /// Finish the already-owned settlement and surface its durable result.
    async fn finish(self: Box<Self>) -> Result<(), BudgetError>;
}

/// Type-erased receipt so legacy budget mocks can keep returning `None`.
pub type BudgetCommitReceipt = Box<dyn BudgetSettlementReceipt>;

/// Budget consultation seam used by `AgentTool` before spawning a subagent.
#[async_trait]
pub trait BudgetEnforcerHandle: Send + Sync {
    /// Charge `nano_usd` against the budget. Returns
    /// [`BudgetError::Exceeded`] (carrying the current cumulative total) if
    /// the post-charge state would exceed the configured limit.
    async fn check_and_charge(&self, nano_usd: u64) -> Result<(), BudgetError>;

    /// Snapshot the current cumulative total (in nano-USD). Used to format
    /// the M3-05 denial string when `check_and_charge` returns
    /// [`BudgetError::Exceeded`] in a context where the caller needs the
    /// number directly.
    async fn snapshot_total_nano_usd(&self) -> u64;

    /// Configured session ceiling, when one exists. Claude Code's Agent tool
    /// includes both the current spend and maximum in its pre-spawn denial.
    /// Defaulted for legacy mocks and unlimited implementations.
    fn max_session_nano_usd(&self) -> Option<u64> {
        None
    }

    /// Return a budget view pinned to an originating session. Background
    /// work may finish after the active conversation changes, so a caller
    /// must not let a late settlement follow the host's current projection.
    /// Legacy implementations return `None` and callers may retain their
    /// existing handle behavior.
    fn scoped_for_session(
        &self,
        _session_id: protocol::SessionId,
    ) -> Option<Arc<dyn BudgetEnforcerHandle>> {
        None
    }

    /// Sum of active reservation holds, in nano-USD. Default 0.
    async fn active_reservation_nano_usd(&self) -> u64 {
        0
    }

    /// Hold `nano_usd` against the session cap until commit/release.
    ///
    /// Default: no max budget (or a zero amount) succeeds with
    /// [`BudgetReservationId::NOOP`]. A configured max without a real
    /// implementation returns [`BudgetError::Internal`] so Fusion can map it to
    /// [`crate::FusionError::BudgetReservationUnavailable`].
    async fn reserve_nano_usd(&self, nano_usd: u64) -> Result<BudgetReservationId, BudgetError> {
        if nano_usd == 0 || self.max_session_nano_usd().is_none() {
            return Ok(BudgetReservationId::NOOP);
        }
        Err(BudgetError::Internal(
            "budget reservation is unimplemented".into(),
        ))
    }

    /// Consume the token after the work has realized `actual_nano_usd`.
    /// The production cost implementation records that externally-priced
    /// amount onto the owning session exactly once; unknown / noop ids succeed.
    async fn commit_reservation(
        &self,
        id: BudgetReservationId,
        actual_nano_usd: u64,
    ) -> Result<(), BudgetError> {
        let _ = actual_nano_usd;
        self.release_reservation(id).await;
        Ok(())
    }

    /// Transfer settlement ownership before any durable queue wait. Legacy
    /// implementations return `None`, preserving their existing commit path;
    /// production implementations return a receipt and disarm the caller's
    /// reservation immediately after `Ok(Some(..))`.
    fn begin_commit_reservation(
        &self,
        _id: BudgetReservationId,
        _actual_nano_usd: u64,
    ) -> Result<Option<BudgetCommitReceipt>, BudgetError> {
        Ok(None)
    }

    /// Drop a hold without realizing additional spend. Unknown / noop ids are
    /// ignored. Implementations must be safe to call from a `Drop` spawned task.
    async fn release_reservation(&self, id: BudgetReservationId) {
        let _ = id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn BudgetEnforcerHandle>> = None;
    }
}
