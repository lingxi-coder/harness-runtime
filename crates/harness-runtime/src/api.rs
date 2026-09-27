//! Transport-independent access to an existing, fully assembled runtime.

use std::path::PathBuf;
use std::sync::Arc;

pub use platform_api::{CostSnapshot, HandleError, OutputStream, TurnOutcome};
pub use protocol::{ConversationMessage, SessionId};
pub use tokio_util::sync::CancellationToken;

/// Input for the existing Agent execution path.
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// User text, passed unchanged to the Agent.
    pub prompt: String,
    /// Images resolved by the existing workspace/model implementation.
    pub images: Vec<PathBuf>,
}

/// Session operations supplied by a product composition.
/// Implementations retain their existing execution, permission and persistence rules.
#[async_trait::async_trait]
pub trait SessionService: Send + Sync {
    /// Execute a turn; output is delivered to the stream supplied at composition.
    async fn run(
        &self,
        input: RunInput,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, HandleError>;
    /// Identity of the currently mounted session.
    async fn session_id(&self) -> SessionId;
    /// Read the existing ordered transcript.
    async fn transcript(&self) -> Vec<ConversationMessage>;
    /// Read cumulative model usage and cost.
    async fn cost(&self) -> CostSnapshot;
}

/// Result of draining the existing runtime resources.
#[derive(Debug, Default)]
pub struct ShutdownReport {
    /// Whether every required shutdown barrier completed.
    pub complete: bool,
    /// Failures retained for a subsequent retry.
    pub errors: Vec<String>,
    /// Durable Fusion publications observed during shutdown.
    pub publications: Vec<platform_api::FusionPublicationReceipt>,
}

/// Lifecycle operations owned by the host composition.
#[async_trait::async_trait]
pub trait LifecycleService: Send + Sync {
    /// Drain resources using the composition's existing shutdown coordinator.
    async fn shutdown(&self) -> ShutdownReport;
}

/// Assembles access to injected services without starting processes or network I/O.
pub struct HarnessBuilder {
    session: Arc<dyn SessionService>,
    lifecycle: Arc<dyn LifecycleService>,
}

impl HarnessBuilder {
    /// Use services from the same runtime composition.
    #[must_use]
    pub fn new(session: Arc<dyn SessionService>, lifecycle: Arc<dyn LifecycleService>) -> Self {
        Self { session, lifecycle }
    }

    /// Retain the supplied services behind the SDK's opaque handles.
    #[must_use]
    pub fn build(self) -> Harness {
        Harness {
            session: SessionHandle {
                service: self.session,
            },
            lifecycle: self.lifecycle,
        }
    }
}

/// An embedded runtime accessed without a client protocol or mutable registries.
/// Hosts must stop admitting work and await outstanding turns before shutdown.
pub struct Harness {
    session: SessionHandle,
    lifecycle: Arc<dyn LifecycleService>,
}

impl Harness {
    /// Access the composition's currently mounted session.
    #[must_use]
    pub fn session(&self) -> SessionHandle {
        self.session.clone()
    }

    /// Drain the runtime. Retry sequentially when the report is incomplete.
    pub async fn shutdown(&self) -> ShutdownReport {
        self.lifecycle.shutdown().await
    }
}

/// Opaque access to session execution and read-only state.
#[derive(Clone)]
pub struct SessionHandle {
    service: Arc<dyn SessionService>,
}

impl SessionHandle {
    /// Run through the existing Agent loop and cancellation handling.
    pub async fn run(
        &self,
        input: RunInput,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, HandleError> {
        self.service.run(input, cancel).await
    }

    /// Identity of the mounted session.
    pub async fn id(&self) -> SessionId {
        self.service.session_id().await
    }

    /// Snapshot of the existing ordered transcript.
    pub async fn transcript(&self) -> Vec<ConversationMessage> {
        self.service.transcript().await
    }

    /// Snapshot of cumulative model usage and cost.
    pub async fn cost(&self) -> CostSnapshot {
        self.service.cost().await
    }
}
