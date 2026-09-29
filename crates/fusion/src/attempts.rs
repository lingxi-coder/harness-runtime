//! Trusted prepare-time registration and owned attempt settlement contracts.
//!
//! Registration captures authority only: it must not reserve budget, acquire
//! model permits, persist dispatch intent, or send provider requests. A host
//! registry retains Weak authorities; an authority must not own its run or
//! derived contexts, which would make a reference cycle.

use std::sync::Arc;

use async_trait::async_trait;
use platform_api::{
    FusionError, FusionInheritance, FusionRequest, FusionRunControl, FusionUsage, ModelAttemptRun,
    ModelAttemptStage,
};

use crate::{FusionRuntimeSnapshot, ResolvedSet};

/// Seal Panel admission and wait for its owned durable receipts while leaving
/// Analyst admission available. Waiting never owns or cancels work.
#[async_trait]
pub trait FusionPanelAttemptFence: Send + Sync {
    fn close(&self);
    async fn wait(&self) -> Result<(), FusionError>;
}

pub(crate) struct CapturedLivePolicy {
    pub control: FusionRunControl,
    pub request: FusionRequest,
    pub resolved: ResolvedSet,
    pub snapshot: Arc<FusionRuntimeSnapshot>,
    pub config: Arc<dyn crate::FusionConfigSource>,
    pub catalog: Arc<dyn crate::ModelSource>,
}

impl FusionAttemptLivePolicy for CapturedLivePolicy {
    fn validate(&self, stage: ModelAttemptStage, slot: Option<u32>) -> Result<(), FusionError> {
        if self.control.cancel().is_cancelled() {
            return Err(FusionError::Cancelled);
        }
        let deadline = self.control.deadline().ok_or_else(|| {
            FusionError::InvalidConfiguration("attempt wire dispatch requires activation".into())
        })?;
        if deadline <= tokio::time::Instant::now() {
            return Err(FusionError::TimedOutEmpty);
        }
        let (route, label, limit, judge) = match (stage, slot) {
            (ModelAttemptStage::Panel, Some(slot)) => (
                self.resolved
                    .panels
                    .get(usize::try_from(slot).map_err(|_| FusionError::Internal)?)
                    .ok_or(FusionError::Internal)?,
                "panel",
                self.snapshot.config.panel_max_output_tokens_per_turn,
                false,
            ),
            (ModelAttemptStage::Analyst, None) => (
                &self.resolved.analyst,
                "analyst",
                self.snapshot.config.analyst_max_output_tokens,
                true,
            ),
            _ => return Err(FusionError::Internal),
        };
        let config_routes = if stage == ModelAttemptStage::Panel {
            self.resolved.panels.iter().collect::<Vec<_>>()
        } else {
            vec![route]
        };
        crate::FusionOrchestrator::ensure_live_config(
            self.config.as_ref(),
            &self.snapshot.config,
            &self.request,
            &config_routes,
            label,
        )?;
        crate::FusionOrchestrator::ensure_live_routes(
            self.catalog.as_ref(),
            &self.snapshot.catalog,
            &[(&route.profile, &route.model, judge)],
            limit,
            label,
        )
    }
}

/// Revalidate captured routes and live restrictions at every wire boundary.
pub trait FusionAttemptLivePolicy: Send + Sync {
    /// Called before authorization and again immediately before transport.
    /// Panel slots are original resolved positions, not completion order.
    fn validate(
        &self,
        stage: ModelAttemptStage,
        panel_slot: Option<u32>,
    ) -> Result<(), FusionError>;
}

/// Fully captured input to a trusted host registrar. Never serialized.
pub struct FusionAttemptRegistration {
    /// Immutable identity, billing mode, cancellation and activation deadline.
    pub control: FusionRunControl,
    /// Captured parent session authority.
    pub inherit: FusionInheritance,
    /// Validated prepared request.
    pub request: FusionRequest,
    /// Original resolved routes and panel order.
    pub resolved: ResolvedSet,
    /// Captured settings, catalog and prices.
    pub snapshot: Arc<FusionRuntimeSnapshot>,
    /// Revalidation against the live sources, without re-resolving routes.
    pub live_policy: Arc<dyn FusionAttemptLivePolicy>,
}

/// Optional host capability. A configured rejection never falls back to legacy.
pub trait FusionAttemptRegistrar: Send + Sync {
    /// Validate host binding and mint a registration without dispatch effects.
    fn register(
        &self,
        captured: FusionAttemptRegistration,
    ) -> Result<RegisteredFusionAttempts, FusionError>;
}

/// Lifetime root plus a finalizer owned by the prepared-run supervisor.
pub struct RegisteredFusionAttempts {
    /// Production hosts fence panel work before the analyst can dispatch.
    pub panel_fence: Option<Arc<dyn FusionPanelAttemptFence>>,
    /// Cloned contexts keep the host authority alive through physical retries.
    pub run: Arc<ModelAttemptRun>,
    /// Consumed only after stage futures and panel producers have drained.
    pub finalizer: Box<dyn FusionAttemptFinalizer>,
}

/// Authoritative facts from the receipt ledger, not stage price estimates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionAttemptSummary {
    /// Includes conservative Unknown occupancy when exact usage is unavailable.
    pub usage: FusionUsage,
    /// Profiles known to have received data.
    pub confirmed_egress: Vec<String>,
    /// Profiles which may have received data.
    pub possible_egress: Vec<String>,
}

/// Failed durability still returns conservative facts; never synthetic zero.
/// The host must freeze its captured accounting authority before returning
/// failure; a UI marker alone is not sufficient to prevent further dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionAttemptSettlementError {
    /// Sanitized failure suitable for terminal facts.
    pub error: FusionError,
    /// Best retained authoritative facts, including unresolved occupancy.
    pub summary: FusionAttemptSummary,
}

/// Must transfer finalization synchronously before returning its waiter.
/// Dropping an unconsumed finalizer must retain host-owned cleanup as well.
pub trait FusionAttemptFinalizer: Send {
    /// Close new attempt admission and take ownership of settling accepted work.
    fn finish(self: Box<Self>) -> Box<dyn FusionAttemptSettlement>;
}

/// A waiter, never the owner of the actual drain/persistence operation.
#[async_trait]
pub trait FusionAttemptSettlement: Send {
    /// Wait for every accepted receipt and its atomic budget publication.
    async fn wait(self: Box<Self>) -> Result<FusionAttemptSummary, FusionAttemptSettlementError>;
}
