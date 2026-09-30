//! Unique physical-send ownership. No clone/serde implementation: a lease has
//! exactly one finalizer, whether its caller finishes, cancels, or disappears.
use super::*;
use crate::{
    AttemptDisposition, AttemptIntent, AttemptReceipt, CostAttemptReceipt, CostPersistError,
};

/// An accepted, durably authorized model attempt. The host still must check
/// live route policy and cancellation immediately before `mark_dispatched`.
pub struct CostBudgetAttempt {
    tracker: Arc<CostTracker>,
    publication: Option<BoundAttemptBudget>,
    profile_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    lifecycle: Arc<crate::tracker::AttemptLifecycle>,
    observation: AttemptReceipt,
    dispatched: bool,
    no_provider_response: bool,
}

impl CostBudgetAttempt {
    pub(crate) fn new(
        intent: &AttemptIntent,
        publication: BoundAttemptBudget,
        profile_permit: tokio::sync::OwnedSemaphorePermit,
        lifecycle: Arc<crate::tracker::AttemptLifecycle>,
    ) -> Self {
        Self {
            tracker: publication.tracker(),
            publication: Some(publication),
            profile_permit: Some(profile_permit),
            lifecycle,
            observation: AttemptReceipt {
                session_id: intent.session_id,
                attempt_id: intent.attempt_id.clone(),
                revision: 1,
                replaces_revision: None,
                disposition: AttemptDisposition::Unknown,
                token_quote_nano_usd: None,
                usage: crate::Usage::default(),
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                api_duration_ms: 0,
                api_duration_without_retries_ms: 0,
            },
            dispatched: false,
            no_provider_response: false,
        }
    }

    /// Record that the transport produced no provider response for this
    /// attempt. Only meaningful once dispatch was marked and nothing was ever
    /// observed; a later observation supersedes it.
    pub fn mark_no_provider_response(&mut self) {
        self.no_provider_response = true;
    }

    /// Mark possible egress without an await between this check and transport.
    /// A mark is not proof that the remote provider accepted a request.
    pub fn mark_dispatched(&mut self) -> Result<(), CostPersistError> {
        self.tracker.preflight_durable()?;
        if self.dispatched {
            return Err(CostPersistError::Rejected(
                "attempt already marked dispatched".into(),
            ));
        }
        self.dispatched = true;
        Ok(())
    }

    /// Retain a cumulative, checked host observation before any schema parse
    /// or await. Observation identity cannot redirect a captured attempt.
    pub fn observe(&mut self, observation: AttemptReceipt) -> Result<(), CostPersistError> {
        if !self.dispatched
            || observation.session_id != self.observation.session_id
            || observation.attempt_id != self.observation.attempt_id
            || observation.revision != 1
            || observation.replaces_revision.is_some()
            || matches!(
                observation.disposition,
                AttemptDisposition::ProvenNotSent | AttemptDisposition::NoProviderResponse
            )
        {
            let error = CostPersistError::Rejected("invalid captured attempt observation".into());
            self.tracker.durability_gate().freeze(error.to_string());
            return Err(error);
        }
        // Once retained, a complete report is not demoted by a later partial
        // framing event. Multiple complete cumulative reports may replace it.
        if self.observation.disposition != AttemptDisposition::Exact
            || observation.disposition == AttemptDisposition::Exact
        {
            self.observation = observation;
        }
        Ok(())
    }

    /// Transfer all resources synchronously; dropping the returned waiter
    /// cannot cancel persistence or prematurely release the profile permit.
    pub fn finish(mut self) -> Result<CostAttemptReceipt, CostPersistError> {
        self.transfer()
    }

    fn transfer(&mut self) -> Result<CostAttemptReceipt, CostPersistError> {
        let publication = self
            .publication
            .take()
            .ok_or_else(|| CostPersistError::Rejected("attempt already finalized".into()))?;
        if !self.dispatched {
            self.observation.disposition = AttemptDisposition::ProvenNotSent;
        } else if self.no_provider_response
            && self.observation.disposition == AttemptDisposition::Unknown
            && self.observation.usage == crate::Usage::default()
        {
            self.observation.disposition = AttemptDisposition::NoProviderResponse;
        }
        let receipt = self.tracker.submit_budgeted_attempt_with_permit(
            self.observation.clone(),
            publication,
            self.profile_permit.take(),
        );
        if let Err(error) = &receipt {
            self.tracker.durability_gate().freeze(error.to_string());
        }
        // Registration above is synchronous. A waiting shutdown can now
        // include this receipt in its next compact slot snapshot/fence.
        self.lifecycle.close();
        receipt
    }
}

impl Drop for CostBudgetAttempt {
    fn drop(&mut self) {
        if self.publication.is_some() {
            let _ = self.transfer();
        }
    }
}

impl BudgetEnforcer {
    /// Acquire one combined hold and persist its intent before returning a
    /// unique send lease. The already-acquired per-profile permit is retained
    /// by an owned worker through cancellation and final settlement.
    pub async fn begin_model_attempt(
        &self,
        intent: AttemptIntent,
        output: &lingxi_core::host::WorkflowOutputScope,
        max_reserved_nano_usd: u64,
        profile_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<CostBudgetAttempt, lingxi_core::host::BudgetError> {
        let publication = self.bind_attempt_budget(output).await?;
        let tracker = publication.tracker();
        let receiver = tracker
            .begin_budgeted_attempt(
                intent,
                publication,
                max_reserved_nano_usd,
                self.config.max_session_nano_usd,
                profile_permit,
            )
            .map_err(|error| lingxi_core::host::BudgetError::Internal(error.to_string()))?;
        receiver.await.map_err(|_| {
            lingxi_core::host::BudgetError::Internal("attempt begin owner disappeared".into())
        })?
    }
}
