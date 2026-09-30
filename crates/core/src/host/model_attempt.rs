//! Host-minted capabilities for registered model calls.
//!
//! Metadata such as a query-source label or JSON stage is never authority. The
//! application must register the opaque identity before accepting it at a wire
//! hook; the capability itself cannot be reconstructed from a request payload.

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Accounting contract selected by trusted host preparation, not request JSON.
/// Serialization supports the durable attempt journal; this enum alone grants
/// no registration or dispatch authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelAttemptBillingMode {
    /// Existing run-level settlement owns money and output publication.
    LegacyAggregate,
    /// Durable physical-attempt receipts own money and output publication.
    MeteredAttempts,
}

/// The paid stage of a registered Fusion run; this metadata grants no rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAttemptStage {
    /// One independent, read-only panel member.
    Panel,
    /// The anonymous structured judge.
    Analyst,
}

/// Process-local registration key. Never persist or emit it in telemetry.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModelAttemptRegistrationId(crate::types::MessageId);

impl fmt::Debug for ModelAttemptRegistrationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelAttemptRegistrationId(<opaque>)")
    }
}

struct Registration {
    id: ModelAttemptRegistrationId,
    next_call: AtomicU64,
    // A host registry stores a Weak to this owner, not another strong root.
    // The owner must not contain this registration or one of its contexts.
    _owner: Arc<dyn Any + Send + Sync>,
}

impl Registration {
    fn next_call_id(&self) -> Result<u64, ModelAttemptContextError> {
        self.next_call
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| ModelAttemptContextError::LogicalCallIdsExhausted)
    }
}

/// Host-owned registration root. Creating this object does not authorize a
/// dispatch: the application's wire hook must recognize its registered ID.
/// It takes no model permits, budget holds or persistence queue capacity.
pub struct ModelAttemptRun {
    registration: Arc<Registration>,
}

impl fmt::Debug for ModelAttemptRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelAttemptRun(<registered host owner>)")
    }
}

impl ModelAttemptRun {
    /// Retain an immutable host authority for the lifetime of all derived
    /// calls. The authority must not own this run/context (no reference cycle).
    #[must_use]
    pub fn new<T: Any + Send + Sync>(owner: Arc<T>) -> Self {
        Self {
            registration: Arc::new(Registration {
                id: ModelAttemptRegistrationId(crate::types::MessageId::new()),
                next_call: AtomicU64::new(1),
                _owner: owner,
            }),
        }
    }

    /// Key used only by the host's live registration table.
    #[must_use]
    pub fn registration_id(&self) -> ModelAttemptRegistrationId {
        self.registration.id
    }

    /// Start one logical call. Retries must clone its returned context instead
    /// of creating another logical call; each actual send gets a new wire ID.
    pub fn context(
        &self,
        stage: ModelAttemptStage,
        panel_slot: Option<u32>,
    ) -> Result<ModelAttemptContext, ModelAttemptContextError> {
        if matches!(stage, ModelAttemptStage::Panel) != panel_slot.is_some() {
            return Err(ModelAttemptContextError::InvalidPanelSlot);
        }
        Ok(ModelAttemptContext {
            logical_call_id: self.registration.next_call_id()?,
            registration: self.registration.clone(),
            stage,
            panel_slot,
        })
    }
}

/// A trusted, non-serialized model-call capability. It must be explicitly
/// propagated by the host; copied labels in prompts or JSON cannot mint one.
///
/// ```compile_fail
/// let _: crate::host::ModelAttemptContext = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone)]
pub struct ModelAttemptContext {
    registration: Arc<Registration>,
    stage: ModelAttemptStage,
    panel_slot: Option<u32>,
    logical_call_id: u64,
}

impl PartialEq for ModelAttemptContext {
    fn eq(&self, other: &Self) -> bool {
        self.registration.id == other.registration.id
            && self.stage == other.stage
            && self.panel_slot == other.panel_slot
            && self.logical_call_id == other.logical_call_id
    }
}

impl Eq for ModelAttemptContext {}

impl fmt::Debug for ModelAttemptContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelAttemptContext")
            .field("stage", &self.stage)
            .field("panel_slot", &self.panel_slot)
            .finish_non_exhaustive()
    }
}

impl ModelAttemptContext {
    /// Borrow the registration identity for a host-table lookup, not logging.
    #[must_use]
    pub fn registration_id(&self) -> ModelAttemptRegistrationId {
        self.registration.id
    }

    /// Immutable originating stage.
    #[must_use]
    pub fn stage(&self) -> ModelAttemptStage {
        self.stage
    }

    /// Panel position captured by the host; absent for judge/synthesis calls.
    #[must_use]
    pub fn panel_slot(&self) -> Option<u32> {
        self.panel_slot
    }

    /// Run-local logical-call ordinal. It is not a wire-attempt identifier.
    #[must_use]
    pub fn logical_call_id(&self) -> u64 {
        self.logical_call_id
    }

    /// Derive the next model round while preserving this stage and panel.
    /// Transport/protocol retries of the same call use `Clone`, not this method.
    pub fn fresh_call(&self) -> Result<Self, ModelAttemptContextError> {
        Ok(Self {
            logical_call_id: self.registration.next_call_id()?,
            ..self.clone()
        })
    }
}

/// Refusal to construct a well-formed logical call; no request was dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ModelAttemptContextError {
    /// Panel calls require a slot, and non-panel calls must not carry one.
    #[error("registered model stage has an invalid panel slot")]
    InvalidPanelSlot,
    /// Checked run-local logical-call counter has no further identifiers.
    #[error("registered model logical-call identifiers are exhausted")]
    LogicalCallIdsExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_are_distinct_and_retries_preserve_logical_identity() {
        let first = ModelAttemptRun::new(Arc::new(()));
        let second = ModelAttemptRun::new(Arc::new(()));
        assert_ne!(first.registration_id(), second.registration_id());
        let call = first.context(ModelAttemptStage::Panel, Some(2)).unwrap();
        let retry = call.clone();
        let next = call.fresh_call().unwrap();
        assert_eq!(retry, call);
        assert_ne!(next, call);
        assert_eq!(retry.registration_id(), call.registration_id());
        assert_eq!(retry.logical_call_id(), call.logical_call_id());
        assert_eq!(next.registration_id(), call.registration_id());
        assert_eq!(next.logical_call_id(), call.logical_call_id() + 1);
        assert_eq!(next.stage(), ModelAttemptStage::Panel);
        assert_eq!(next.panel_slot(), Some(2));
    }

    #[test]
    fn parallel_rounds_never_reuse_a_logical_call_id() {
        let run = ModelAttemptRun::new(Arc::new(()));
        let first = run.context(ModelAttemptStage::Panel, Some(0)).unwrap();
        let workers = (0..4)
            .map(|_| {
                let call = first.clone();
                std::thread::spawn(move || {
                    (0..16)
                        .map(|_| call.fresh_call().unwrap().logical_call_id())
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let ids = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 64);
        assert!(!ids.contains(&first.logical_call_id()));
    }

    #[test]
    fn contexts_retain_the_host_owner_without_a_registry_strong_root() {
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let run = ModelAttemptRun::new(owner);
        let call = run.context(ModelAttemptStage::Analyst, None).unwrap();
        drop(run);
        assert!(weak.upgrade().is_some());
        drop(call);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn stage_slot_mismatch_and_identifier_exhaustion_fail_closed() {
        let run = ModelAttemptRun::new(Arc::new(()));
        assert!(matches!(
            run.context(ModelAttemptStage::Panel, None),
            Err(ModelAttemptContextError::InvalidPanelSlot)
        ));
        assert!(matches!(
            run.context(ModelAttemptStage::Analyst, Some(0)),
            Err(ModelAttemptContextError::InvalidPanelSlot)
        ));
        run.registration
            .next_call
            .store(u64::MAX - 1, Ordering::Relaxed);
        let last = run.context(ModelAttemptStage::Analyst, None).unwrap();
        assert_eq!(last.logical_call_id(), u64::MAX - 1);
        assert!(matches!(
            last.fresh_call(),
            Err(ModelAttemptContextError::LogicalCallIdsExhausted)
        ));
        assert_eq!(run.registration.next_call.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn debug_never_exposes_registration_or_host_owner() {
        let run = ModelAttemptRun::new(Arc::new("private host authority"));
        let call = run.context(ModelAttemptStage::Analyst, None).unwrap();
        assert_eq!(
            format!("{:?}", run.registration_id()),
            "ModelAttemptRegistrationId(<opaque>)"
        );
        assert!(!format!("{run:?} {call:?}").contains("private host authority"));
        assert!(!format!("{call:?}").contains("logical_call_id"));
    }
}
