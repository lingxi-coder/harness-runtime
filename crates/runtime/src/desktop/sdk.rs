//! SDK access backed by the existing desktop composition and shutdown owner.

use super::{BuildError, DesktopConfig, DesktopRuntime};
use crate::api::{
    CancellationToken, ConversationMessage, CostSnapshot, HandleError, Harness, HarnessBuilder,
    LifecycleService, OutputStream, RunInput, SessionId, SessionService, ShutdownReport,
    TurnOutcome,
};
use permission::gate::PermissionGate;
use platform_api::OrchestratorHandle;
use std::sync::Arc;

/// Assemble the existing product capabilities for an embedded Rust host.
/// Startup has the same explicit I/O and configuration semantics as [`super::build`].
pub async fn build_harness(
    mut config: DesktopConfig,
    output: Arc<dyn OutputStream>,
    permissions: Arc<dyn PermissionGate>,
) -> Result<Harness, BuildError> {
    config.injected_permission_gate = Some(permissions);
    let runtime = Arc::new(super::build(config, output, Arc::new(UnusedPermissionSink)).await?);
    Ok(HarnessBuilder::new(runtime.clone(), runtime).build())
}

#[async_trait::async_trait]
impl SessionService for DesktopRuntime {
    async fn run(
        &self,
        input: RunInput,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, HandleError> {
        OrchestratorHandle::run_turn_streaming_with_images(
            self.orchestrator.as_ref(),
            &input.prompt,
            &input.images,
            cancel,
        )
        .await
    }

    async fn session_id(&self) -> SessionId {
        self.orchestrator.current_session_id().await
    }

    async fn transcript(&self) -> Vec<ConversationMessage> {
        self.orchestrator.conversation_transcript().await
    }

    async fn cost(&self) -> CostSnapshot {
        self.orchestrator.snapshot_cost().await
    }
}

#[async_trait::async_trait]
impl LifecycleService for DesktopRuntime {
    async fn shutdown(&self) -> ShutdownReport {
        let report = self.session_lifecycle.shutdown_and_drain().await;
        ShutdownReport {
            complete: report.complete,
            errors: report.errors,
            publications: report.publications,
        }
    }
}

struct UnusedPermissionSink;

#[async_trait::async_trait]
impl client::adapter::PermissionRequestSink for UnusedPermissionSink {
    async fn emit_request(&self, _request: client::protocol::permission::PermissionRequest) {
        unreachable!("embedded hosts use their injected permission gate");
    }
}
