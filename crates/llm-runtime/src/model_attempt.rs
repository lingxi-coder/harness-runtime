//! App-owned accounting hooks at the physical model transport boundary.
//!
//! This crate does not own prices or ledgers. A registered request requires an
//! installed host hook; a query-source string is never a substitute. Ordinary
//! requests without a capability keep their existing transport behavior.

use async_trait::async_trait;
use lingxi_core::host::ModelAttemptContext;

use crate::{ExecutionUsage, LlmError, LlmRequest, PreparedLlmCall};

/// Whether normalized usage is a partial observation or an explicitly complete
/// provider report. Successful transport alone does not prove complete usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAttemptUsageCompleteness {
    /// Preserve known counters while retaining conservative unknown occupancy.
    Partial,
    /// The provider supplied its final usage; schema success is irrelevant.
    Complete,
}

/// One application implementation shared by a service's registered Fusion
/// calls. Admission order is profile permit, queue capacity, atomic budget
/// authorization, durable intent, and live policy recheck before dispatch.
#[async_trait]
pub trait ModelAttemptHooks: Send + Sync {
    /// Read the registered origin before provider preparation. The opaque
    /// registration is authority; model payload fields never supply this ID.
    fn request_session_id(
        &self,
        _context: &ModelAttemptContext,
    ) -> Option<lingxi_core::types::SessionId> {
        None
    }
    /// Prepare exactly one physical attempt after route, body and headers are
    /// final, but before invoking transport. The host must verify this context
    /// belongs to a live registered authority and this exact captured route.
    /// Neither request bodies nor credential headers may be logged by hooks.
    async fn begin(
        &self,
        context: &ModelAttemptContext,
        request: &LlmRequest,
        prepared: &PreparedLlmCall,
    ) -> Result<Box<dyn ModelAttemptLease>, LlmError>;
}

/// One accepted physical attempt. Implementations must retain a conservative
/// settlement when dropped: before dispatch it is proven-not-sent, afterwards
/// it is incomplete unless an actual complete observation has been retained.
/// The lease owns its profile permit and originating-session authority.
pub trait ModelAttemptLease: Send {
    /// Optional originating-session observer for calls outside a query task.
    /// It observes response facts and never affects budget admission.
    fn model_safety_observer(
        &self,
    ) -> Option<lingxi_core::host::model_safety::ModelSafetyObserver> {
        None
    }
    /// Synchronous final live-policy/freeze check and dispatch marker. Invoke
    /// immediately before transport, with no intervening await. A marker is
    /// not proof that the remote service accepted or billed the request.
    fn mark_dispatched(&mut self) -> Result<(), LlmError>;

    /// Retain cumulative normalized facts before yielding them to consumers or
    /// parsing a Fusion/structured-output schema. This method must not await.
    /// Repeated cumulative snapshots replace earlier observations, not add to
    /// them. Content, raw source text and credentials are not part of this seam.
    fn observe_usage(
        &mut self,
        usage: &ExecutionUsage,
        completeness: ModelAttemptUsageCompleteness,
    );

    /// Record that no provider response was ever accepted for this attempt.
    /// This does not prove the provider did not execute the request. Hosts
    /// must retain unknown budget occupancy even when no response was received.
    fn mark_no_provider_response(&mut self) {}

    /// Synchronously transfer the observation and permit into a host-owned
    /// finalizer, then return a waiter. Dropping that waiter cannot discard
    /// usage, cancel accepted persistence or release a reservation prematurely.
    fn finish(self: Box<Self>) -> Box<dyn ModelAttemptSettlement>;
}

/// Waiter for an already-owned attempt settlement. A failure must remain
/// visible to the originating session and must not authorize another attempt.
#[async_trait]
pub trait ModelAttemptSettlement: Send {
    /// Observe the retained durable result; the work already has an owner.
    async fn wait(self: Box<Self>) -> Result<(), LlmError>;
}

/// Transport-owned observation state. Dropping it leaves finalization to the
/// host lease's required Drop contract; explicit finish transfers synchronously.
pub(crate) struct WireAttempt {
    lease: Option<Box<dyn ModelAttemptLease>>,
    usage: ExecutionUsage,
}

impl WireAttempt {
    pub(crate) fn model_safety_observer(
        &self,
    ) -> Option<lingxi_core::host::model_safety::ModelSafetyObserver> {
        self.lease
            .as_ref()
            .and_then(|lease| lease.model_safety_observer())
    }
    pub(crate) fn new(lease: Option<Box<dyn ModelAttemptLease>>) -> Self {
        Self {
            lease,
            usage: ExecutionUsage::default(),
        }
    }

    pub(crate) fn mark_dispatched(&mut self) -> Result<(), LlmError> {
        match self.lease.as_mut() {
            Some(lease) => lease.mark_dispatched().map_err(accounting_error),
            None => Ok(()),
        }
    }

    pub(crate) fn observe(
        &mut self,
        usage: &ExecutionUsage,
        completeness: ModelAttemptUsageCompleteness,
    ) {
        self.usage = usage.clone();
        if let Some(lease) = self.lease.as_mut() {
            lease.observe_usage(&self.usage, completeness);
        }
    }

    pub(crate) fn observe_events(&mut self, events: &[crate::HistoryEvent]) {
        if self.lease.is_none() {
            return;
        }
        for event in events {
            match event {
                crate::HistoryEvent::MessageStart { response } => {
                    self.observe(&response.usage, ModelAttemptUsageCompleteness::Partial);
                }
                crate::HistoryEvent::MessageDelta {
                    usage: Some(usage), ..
                } => {
                    let merged = self.usage.merge_snapshot(usage);
                    self.observe(
                        &merged,
                        if merged.report.complete().is_some() {
                            ModelAttemptUsageCompleteness::Complete
                        } else {
                            ModelAttemptUsageCompleteness::Partial
                        },
                    );
                }
                crate::HistoryEvent::Completed { response } => {
                    if has_usage_report(&response.usage) {
                        self.observe(&response.usage, ModelAttemptUsageCompleteness::Complete);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) async fn finish(&mut self) -> Result<(), LlmError> {
        if let Some(lease) = self.lease.take() {
            // Missing usage cannot establish whether a dispatched request ran.
            // Leave the lease unknown unless actual provider usage was observed.
            lease.finish().wait().await.map_err(accounting_error)?;
        }
        Ok(())
    }
}

/// A complete SDK report is the only authority for exact settlement.
/// Provider-specific field validation belongs to the SDK codec.
pub(crate) fn has_usage_report(usage: &ExecutionUsage) -> bool {
    usage.report.complete().is_some()
}

pub(crate) fn missing_hooks_error() -> LlmError {
    LlmError::InvalidRequest {
        message: "registered model attempt requires host accounting hooks".into(),
    }
}

pub(crate) fn accounting_error(error: LlmError) -> LlmError {
    LlmError::CostUnavailable {
        message: format!("registered attempt accounting failed: {error}"),
    }
}
