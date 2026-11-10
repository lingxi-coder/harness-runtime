//! Admission-time suppression for Native's ambient Dream lifecycle. The
//! worker deliberately finishes as soon as the registry activates it.

use async_trait::async_trait;
use lingxi_core::host::task_delivery::TaskDeliveryPolicy;
use lingxi_core::host::FileSystem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use tasks::output_manager::TaskOutputManager;
use tasks::registry::TaskRegistry;
use tasks::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use tasks::{TaskStatus, TaskType};
use test_harness::mocks::MockRuntimeSpawner;
mod spool_fs {
    include!("support/auto_dream_fs.rs");
}
use spool_fs::InMemoryFs;

struct InstantDream {
    registry: Mutex<Weak<TaskRegistry>>,
    status: TaskStatus,
    completed: Mutex<Option<tokio::sync::oneshot::Sender<(bool, usize)>>>,
}

#[async_trait]
impl Task for InstantDream {
    fn name(&self) -> &str {
        "instant-dream"
    }
    fn task_type(&self) -> TaskType {
        TaskType::Dream
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        context: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        assert!(matches!(input, TaskSpawnInput::Dream { .. }));
        let registry = self.registry.lock().unwrap().upgrade().unwrap();
        let completed = self.completed.lock().unwrap().take().unwrap();
        let status = self.status;
        let id = "dinstant1";
        let (activate, ready) = tokio::sync::oneshot::channel();
        context
            .runtime
            .spawn(
                "instant-dream-worker",
                Box::pin(async move {
                    ready.await.unwrap();
                    let notified_before_completion =
                        registry.get(id).await.unwrap().base().notified;
                    // No model work, file write or artificial delay: completion can
                    // race the scheduler's return from TaskRegistry::spawn.
                    registry.set_status(id, status).await.unwrap();
                    let notices = registry
                        .take_pending_task_notifications(TaskDeliveryPolicy::default())
                        .await;
                    let _ = completed.send((notified_before_completion, notices.len()));
                }),
            )
            .await
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        Ok(TaskHandle::new(id, None).with_activation(move || {
            let _ = activate.send(());
        }))
    }

    async fn kill(&self, _id: &str, _context: TaskContext) -> Result<(), TaskError> {
        Ok(())
    }
}

async fn immediate_terminal_run(automatic: bool, terminal: TaskStatus) {
    let filesystem: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let mut registry = TaskRegistry::new(
        runtime,
        filesystem.clone(),
        Arc::new(TaskOutputManager::new(
            PathBuf::from("/task-output"),
            filesystem,
        )),
    );
    let (completed, observed) = tokio::sync::oneshot::channel();
    let handler = Arc::new(InstantDream {
        registry: Mutex::new(Weak::new()),
        status: terminal,
        completed: Mutex::new(Some(completed)),
    });
    registry.register_handler(TaskType::Dream, handler.clone());
    let registry = Arc::new(registry);
    *handler.registry.lock().unwrap() = Arc::downgrade(&registry);
    let task = registry
        .spawn(
            TaskType::Dream,
            TaskSpawnInput::Dream {
                prompt: "explicit consolidation directive".into(),
                context: None,
                fork_request: None,
                fork_wire: None,
                writes: automatic.then(|| Arc::new(Mutex::new(Vec::new()))),
                max_iterations: None,
            },
            "dreaming".into(),
        )
        .await
        .unwrap();
    let (notified_at_activation, generic_notice_count) = observed.await.unwrap();
    assert_eq!(
        notified_at_activation, automatic,
        "notification policy must be installed before activation"
    );
    assert_eq!(generic_notice_count, usize::from(!automatic));
    assert_eq!(
        registry.get(&task).await.unwrap().base().status,
        terminal,
        "the terminal row remains until its completion owner can consume it"
    );
    assert!(registry
        .take_pending_task_notifications(TaskDeliveryPolicy::default())
        .await
        .is_empty());
    if automatic {
        assert!(registry.get(&task).await.is_some(), "ambient rows must survive generic notification sweeps until the scheduler consumes them");
    }
}

#[tokio::test]
async fn automatic_dream_is_ambient_before_instant_completion_or_failure() {
    for terminal in [
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Killed,
    ] {
        immediate_terminal_run(true, terminal).await;
    }
}

#[tokio::test]
async fn manual_dream_retains_ordinary_model_task_notifications() {
    immediate_terminal_run(false, TaskStatus::Completed).await;
}
