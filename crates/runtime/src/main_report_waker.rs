//! Host scheduling for already-admitted peer reports. Wake markers carry no
//! report body and never become human prompts.

#[cfg(feature = "mobile")]
use std::sync::RwLock;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use lingxi_core::host::handback::HandbackSessionScope;
use lingxi_core::host::orchestrator::MainReportWaker;
use lingxi_core::types::MessageId;
use orchestrator::ConversationOrchestrator;

pub(crate) struct DirectMainReportWaker {
    orchestrator: Weak<ConversationOrchestrator>,
}

impl DirectMainReportWaker {
    pub(crate) fn new(orchestrator: &Arc<ConversationOrchestrator>) -> Self {
        Self {
            orchestrator: Arc::downgrade(orchestrator),
        }
    }
}

#[async_trait]
impl MainReportWaker for DirectMainReportWaker {
    async fn wake(&self, scope: HandbackSessionScope, _message_id: MessageId) {
        let target = self.orchestrator.clone();
        tokio::spawn(async move {
            let Some(orchestrator) = target.upgrade() else {
                return;
            };
            if let Err(error) = orchestrator
                .run_main_report_turn(scope, tokio_util::sync::CancellationToken::new())
                .await
            {
                tracing::warn!(%error, "subagent report wake failed");
            }
        });
    }
}

/// The embedded mobile runtime can schedule its own reports. Its native host
/// replaces the delegate with the same queue and turn slot used by other wakes.
#[cfg(feature = "mobile")]
pub(crate) struct MainReportWakeRouter {
    delegate: RwLock<Arc<dyn MainReportWaker>>,
}

#[cfg(feature = "mobile")]
impl MainReportWakeRouter {
    pub(crate) fn new(delegate: Arc<dyn MainReportWaker>) -> Self {
        Self {
            delegate: RwLock::new(delegate),
        }
    }

    pub(crate) fn bind_queue(&self, queue: Arc<msgqueue::MessageQueueManager>) {
        *self
            .delegate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::new(QueuedMainReportWaker(queue));
    }
}

#[cfg(feature = "mobile")]
#[async_trait]
impl MainReportWaker for MainReportWakeRouter {
    async fn wake(&self, scope: HandbackSessionScope, message_id: MessageId) {
        let delegate = self
            .delegate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        delegate.wake(scope, message_id).await;
    }
}

#[cfg(feature = "mobile")]
struct QueuedMainReportWaker(Arc<msgqueue::MessageQueueManager>);

#[cfg(feature = "mobile")]
#[async_trait]
impl MainReportWaker for QueuedMainReportWaker {
    async fn wake(&self, scope: HandbackSessionScope, message_id: MessageId) {
        self.0.enqueue_handback_wake(scope, message_id).await;
    }
}
