//! Exercise the supported Rust SDK from a separate crate using runtime paths.

use harness_runtime::{
    CancellationToken, ConversationMessage, CostSnapshot, HandleError, HarnessBuilder,
    LifecycleService, RunInput, SessionId, SessionService, ShutdownReport, TurnOutcome,
};
use std::sync::Arc;

struct Session(SessionId);

#[async_trait::async_trait]
impl SessionService for Session {
    async fn run(
        &self,
        _input: RunInput,
        _cancel: CancellationToken,
    ) -> Result<TurnOutcome, HandleError> {
        unreachable!("this test checks the public surface and lifecycle")
    }

    async fn session_id(&self) -> SessionId {
        self.0.clone()
    }

    async fn transcript(&self) -> Vec<ConversationMessage> {
        Vec::new()
    }

    async fn cost(&self) -> CostSnapshot {
        unreachable!("this test does not request usage")
    }
}

struct Lifecycle;

#[async_trait::async_trait]
impl LifecycleService for Lifecycle {
    async fn shutdown(&self) -> ShutdownReport {
        ShutdownReport {
            complete: true,
            ..ShutdownReport::default()
        }
    }
}

#[tokio::test]
async fn runtime_root_exposes_the_host_session_contract() {
    let id = SessionId::new();
    let harness = HarnessBuilder::new(Arc::new(Session(id.clone())), Arc::new(Lifecycle)).build();
    assert_eq!(harness.session().id().await, id);
    assert!(harness.shutdown().await.complete);
}
