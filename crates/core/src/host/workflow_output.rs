//! Captured, process-local output accounting for a session turn.
//!
//! All workflows in one turn share an account. An old scope never rebinds to
//! a newer session or generation. Cost implementations update their existing
//! reservation book and the derived snapshot under the same book lock; they
//! must not invoke user callbacks while holding that lock.

use std::{fmt, sync::Arc};

use crate::types::{MessageId, SessionId};
use async_trait::async_trait;

use crate::host::BudgetError;

/// Stable identities for legacy aggregate output; registered attempt output
/// is published by its receipt instead and must not also use this path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowOutputEventId {
    /// One main-loop response.
    MainResponse(MessageId),
    /// One workflow agent call.
    WorkflowAgent {
        /// Stable workflow run identity.
        run_id: String,
        /// Stable call index within that run.
        call_index: u64,
    },
}

/// Host implementation of one immutable session/turn binding.
pub trait WorkflowOutputAccount: Send + Sync {
    /// Canonical origin session, never the currently selected session.
    fn session_id(&self) -> SessionId;
    /// Origin turn identity, fixed for this account's lifetime.
    fn generation_id(&self) -> MessageId;
    /// Derived spent snapshot, including conservative unknown contributions
    /// but excluding outstanding holds.
    fn spent(&self) -> u64;
    /// Checked, synchronous publication. Identical event replay is a no-op;
    /// conflicting replay and overflow fail without changing the account.
    /// Call immediately after obtaining output, before any intervening await.
    fn record_legacy(
        &self,
        event: WorkflowOutputEventId,
        output_tokens: u64,
    ) -> Result<(), BudgetError>;
}

/// Non-serialized capability constructed by trusted Rust hosts or mocks.
/// Clones retain the exact original account, even after another turn begins.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<crate::host::WorkflowOutputScope>();
/// ```
///
/// ```compile_fail
/// fn requires_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// requires_deserialize::<crate::host::WorkflowOutputScope>();
/// ```
#[derive(Clone)]
pub struct WorkflowOutputScope(
    Arc<dyn WorkflowOutputAccount>,
    Option<crate::host::SessionRetentionPin>,
);

impl WorkflowOutputScope {
    /// Bind a trusted host account. JSON and metadata cannot mint this scope.
    #[must_use]
    pub fn new(account: Arc<dyn WorkflowOutputAccount>) -> Self {
        Self(account, None)
    }

    /// Attach the real account authority's external-view pin. Cache-owned
    /// account cores must not retain this view; clones keep the pin alive.
    #[must_use]
    pub fn with_retention_pin(mut self, pin: crate::host::SessionRetentionPin) -> Self {
        self.1 = Some(pin);
        self
    }

    /// Whether both scopes retain the exact same account allocation.
    /// This invokes no account methods: matching session/turn metadata alone
    /// does not establish account authority.
    #[must_use]
    pub fn shares_account(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Canonical origin session.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.0.session_id()
    }

    /// Fixed origin turn identity.
    #[must_use]
    pub fn generation_id(&self) -> MessageId {
        self.0.generation_id()
    }

    /// Derived spent snapshot, excluding holds.
    #[must_use]
    pub fn spent(&self) -> u64 {
        self.0.spent()
    }

    /// Publish without an await/cancellation window. Call before awaiting
    /// anything else after obtaining the legacy output.
    pub fn record_legacy(
        &self,
        event: WorkflowOutputEventId,
        output_tokens: u64,
    ) -> Result<(), BudgetError> {
        self.0.record_legacy(event, output_tokens)
    }
}

impl fmt::Debug for WorkflowOutputScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkflowOutputScope(<opaque>)")
    }
}

/// Host-owned current-scope publication. Capturing does not create a turn.
#[async_trait]
pub trait WorkflowOutputScopes: Send + Sync {
    /// Return the existing session account unchanged, or atomically publish
    /// an initial command-only generation when none exists. Never reinterpret
    /// a durability/identity error as absence. Hosts call this before exposing
    /// a newly activated session to workflows that can run before a model turn.
    /// The default is fail-closed for providers without initialization support.
    async fn ensure_current(
        &self,
        session_id: SessionId,
        _initial_generation: MessageId,
        _max_output_tokens: Option<u64>,
    ) -> Result<WorkflowOutputScope, BudgetError> {
        self.capture(session_id)
    }

    /// Begin or retrieve the shared account for this session and generation.
    /// Conflicting limits for an existing generation must fail, not reset it.
    async fn begin_turn(
        &self,
        session_id: SessionId,
        generation_id: MessageId,
        max_output_tokens: Option<u64>,
    ) -> Result<WorkflowOutputScope, BudgetError>;

    /// Capture the published scope for exactly this session; missing or
    /// mismatched sessions fail rather than falling back to another session.
    fn capture(&self, session_id: SessionId) -> Result<WorkflowOutputScope, BudgetError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };

    struct Account {
        session: SessionId,
        generation: MessageId,
        spent: AtomicU64,
        events: Mutex<Vec<(WorkflowOutputEventId, u64)>>,
    }

    impl WorkflowOutputAccount for Account {
        fn session_id(&self) -> SessionId {
            self.session
        }
        fn generation_id(&self) -> MessageId {
            self.generation
        }
        fn spent(&self) -> u64 {
            self.spent.load(Ordering::Relaxed)
        }
        fn record_legacy(
            &self,
            event: WorkflowOutputEventId,
            tokens: u64,
        ) -> Result<(), BudgetError> {
            if tokens == u64::MAX {
                return Err(BudgetError::Internal("probe rejection".into()));
            }
            self.events.lock().unwrap().push((event, tokens));
            self.spent.fetch_add(tokens, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn workflow_output_scope_delegates_and_retains_captured_identity() {
        let account = Arc::new(Account {
            session: SessionId::new(),
            generation: MessageId::new(),
            spent: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
        });
        let scope = WorkflowOutputScope::new(account.clone());
        let captured = scope.clone();
        assert!(scope.shares_account(&captured));
        let same_ids = WorkflowOutputScope::new(Arc::new(Account {
            session: account.session,
            generation: account.generation,
            spent: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
        }));
        assert_eq!(scope.session_id(), same_ids.session_id());
        assert_eq!(scope.generation_id(), same_ids.generation_id());
        assert!(!scope.shares_account(&same_ids));
        // Replacing the host's current scope cannot redirect a captured clone.
        let current = WorkflowOutputScope::new(Arc::new(Account {
            session: SessionId::new(),
            generation: MessageId::new(),
            spent: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
        }));
        let event = WorkflowOutputEventId::MainResponse(MessageId::new());
        scope.record_legacy(event.clone(), 17).unwrap();
        assert_eq!(captured.session_id(), account.session);
        assert_eq!(captured.generation_id(), account.generation);
        assert_eq!(captured.spent(), 17);
        assert_ne!(captured.session_id(), current.session_id());
        assert_ne!(captured.generation_id(), current.generation_id());
        assert_eq!(current.spent(), 0);
        assert_eq!(*account.events.lock().unwrap(), vec![(event.clone(), 17)]);
        assert!(captured.record_legacy(event, u64::MAX).is_err());
        assert_eq!(captured.spent(), 17);
        assert_eq!(format!("{captured:?}"), "WorkflowOutputScope(<opaque>)");
    }
}
