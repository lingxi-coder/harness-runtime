//! `BudgetEnforcerHandle` trait impl for `BudgetEnforcer`.
//!
//! Bridges the `lingxi-traits` seam to the concrete M3-05 `BudgetEnforcer`
//! so `AgentTool` in `lingxi-tools` can consult the parent's budget without
//! taking a cyclic dep on `lingxi-cost`.
//!
//! The `BudgetError::Exceeded { current_nano_usd }` branch flows the
//! cumulative cost back to the caller together with the configured ceiling,
//! allowing AgentTool to render Claude's pre-spawn budget-limit denial.

use crate::budget::{BudgetCheckResult, BudgetEnforcer};
use async_trait::async_trait;
use platform_api::budget::{BudgetEnforcerHandle, BudgetError};
use protocol::SessionId;
use std::sync::Arc;

#[async_trait]
impl BudgetEnforcerHandle for BudgetEnforcer {
    async fn check_and_charge(&self, nano_usd: u64) -> Result<(), BudgetError> {
        match self.check_pre_api_call(nano_usd).await {
            BudgetCheckResult::Ok
            | BudgetCheckResult::ThresholdWarning { .. }
            | BudgetCheckResult::Warn { .. } => Ok(()),
            BudgetCheckResult::Halt { current, .. }
            | BudgetCheckResult::AskUser { current, .. } => Err(BudgetError::Exceeded {
                current_nano_usd: current,
            }),
            BudgetCheckResult::Unavailable { reason } => Err(BudgetError::Internal(reason)),
        }
    }

    async fn snapshot_total_nano_usd(&self) -> u64 {
        self.cost_tracker_arc().total_nano_usd().await
    }

    fn max_session_nano_usd(&self) -> Option<u64> {
        BudgetEnforcer::max_session_nano_usd(self)
    }

    fn scoped_for_session(&self, session_id: SessionId) -> Option<Arc<dyn BudgetEnforcerHandle>> {
        Some(BudgetEnforcer::scoped_for_session(self, session_id))
    }

    async fn active_reservation_nano_usd(&self) -> u64 {
        BudgetEnforcer::active_reservation_nano_usd(self).await
    }

    async fn reserve_nano_usd(
        &self,
        nano_usd: u64,
    ) -> Result<platform_api::BudgetReservationId, BudgetError> {
        BudgetEnforcer::reserve_nano_usd(self, nano_usd).await
    }

    async fn commit_reservation(
        &self,
        id: platform_api::BudgetReservationId,
        actual_nano_usd: u64,
    ) -> Result<(), BudgetError> {
        BudgetEnforcer::commit_reservation(self, id, actual_nano_usd).await
    }

    fn begin_commit_reservation(
        &self,
        id: platform_api::BudgetReservationId,
        actual_nano_usd: u64,
    ) -> Result<Option<platform_api::BudgetCommitReceipt>, BudgetError> {
        BudgetEnforcer::begin_commit_reservation(self, id, actual_nano_usd)
    }

    async fn release_reservation(&self, id: platform_api::BudgetReservationId) {
        BudgetEnforcer::release_reservation(self, id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{PricingCatalog, ProviderId};
    use crate::tracker::CostTracker;
    use crate::usage::{TokenUsage, Usage};
    use crate::{BudgetConfig, BudgetExceedPolicy, ModelRef};
    use protocol::SessionId;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn make_tracker() -> Arc<CostTracker> {
        let (tx, _rx) = mpsc::channel(8);
        Arc::new(CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ))
    }

    #[tokio::test]
    async fn check_and_charge_under_budget_is_ok() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000_000_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let enforcer = BudgetEnforcer::new(cfg, make_tracker());
        let h: &dyn BudgetEnforcerHandle = &enforcer;
        assert!(h.check_and_charge(0).await.is_ok());
    }

    #[tokio::test]
    async fn check_and_charge_over_budget_returns_exceeded_with_current() {
        let tracker = make_tracker();
        // Force tracker to register some cost.
        tracker
            .record_api_response_v2(
                ModelRef {
                    provider: ProviderId::Anthropic,
                    model: "claude-opus-4-6".into(),
                },
                Usage {
                    tokens: TokenUsage {
                        input: 300_000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(1),
                0,
                0,
                0,
                false,
                None,
            )
            .await;
        // Cap below the tracked total.
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000_000_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let enforcer = BudgetEnforcer::new(cfg, tracker);
        let h: &dyn BudgetEnforcerHandle = &enforcer;
        let err = h.check_and_charge(0).await.unwrap_err();
        match err {
            BudgetError::Exceeded { current_nano_usd } => {
                assert!(current_nano_usd > 1_000_000_000);
            }
            BudgetError::Internal(s) => panic!("expected Exceeded, got Internal({s})"),
        }
    }

    #[tokio::test]
    async fn snapshot_total_returns_zero_initially() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000_000_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let enforcer = BudgetEnforcer::new(cfg, make_tracker());
        let h: &dyn BudgetEnforcerHandle = &enforcer;
        assert_eq!(h.snapshot_total_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn handle_reserve_commit_release_roundtrip() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(5_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let enforcer = BudgetEnforcer::new(cfg, make_tracker());
        let h: &dyn BudgetEnforcerHandle = &enforcer;
        let id = h.reserve_nano_usd(1_000).await.unwrap();
        assert_eq!(h.active_reservation_nano_usd().await, 1_000);
        h.commit_reservation(id, 400).await.unwrap();
        assert_eq!(h.active_reservation_nano_usd().await, 0);
        let id = h.reserve_nano_usd(1_000).await.unwrap();
        h.release_reservation(id).await;
        assert_eq!(h.active_reservation_nano_usd().await, 0);
    }
}
