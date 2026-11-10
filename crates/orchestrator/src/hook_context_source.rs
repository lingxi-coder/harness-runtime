//! Session ownership for hook adapters assembled before the orchestrator.

use crate::conversation::ConversationOrchestrator;
use hooks::HookContext;
use lingxi_core::types::SessionId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Resolves current events against the live conversation and keeps only the
/// immutable owner facts needed by a registered task's eventual completion.
pub struct HookContextSource {
    initial: HookContext,
    orchestrator: OnceLock<Weak<ConversationOrchestrator>>,
    task_origins: Mutex<HashMap<String, HookContext>>,
}

impl HookContextSource {
    #[must_use]
    pub fn new(initial: HookContext) -> Self {
        Self {
            initial,
            orchestrator: OnceLock::new(),
            task_origins: Mutex::new(HashMap::new()),
        }
    }

    pub fn attach(&self, orchestrator: &Arc<ConversationOrchestrator>) {
        let _ = self.orchestrator.set(Arc::downgrade(orchestrator));
    }

    pub(crate) fn from_paths(cwd: PathBuf, transcript_path: PathBuf) -> Self {
        let session_id = transcript_path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(SessionId::parse_prefixed)
            .unwrap_or_default();
        Self::new(HookContext {
            session_id,
            cwd,
            transcript_path,
            ..HookContext::default()
        })
    }

    pub(crate) async fn current(&self) -> Option<HookContext> {
        let Some(orchestrator) = self.orchestrator.get().and_then(Weak::upgrade) else {
            return Some(self.initial.clone());
        };
        let owner = orchestrator.lifecycle_runtime.mod_session_jobs.capture()?;
        let (_, lock) = orchestrator
            .lifecycle_runtime
            .session_tool_hook_generation
            .current();
        let mut context = orchestrator.expansion_hook_context().await;
        if owner.is_cancelled()
            || orchestrator.session.lock().await.session_id != context.session_id
        {
            return None;
        }
        context.publication_guard = Some(Arc::new(
            crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(owner, lock)
                .with_transient_publications(
                    orchestrator
                        .lifecycle_runtime
                        .session_tool_hook_generation
                        .transient_publications(),
                ),
        ));
        Some(context)
    }

    pub(crate) async fn capture_task_origin(&self, task_id: &str) -> Option<HookContext> {
        if let Some(origin) = self
            .task_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .cloned()
        {
            return Some(origin);
        }
        let mut context = self.current().await?;
        // A task can outlive the foreground history. Its evaluator loads the
        // originating transcript path instead of retaining an entire history.
        context.prompt_transcript = None;
        context.last_assistant_message = None;
        let origin = self
            .task_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(task_id.to_owned())
            .or_insert(context)
            .clone();
        Some(origin)
    }

    pub(crate) async fn task_completed(&self, task_id: &str) -> Option<HookContext> {
        let origin = self
            .task_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(task_id);
        match origin {
            Some(origin) => Some(origin),
            None => self.current().await,
        }
    }

    pub(crate) fn discard_task_origin(&self, task_id: &str) {
        self.task_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(task_id);
    }

    pub(crate) async fn for_session(&self, session_id: SessionId) -> Option<HookContext> {
        if self.orchestrator.get().is_none() {
            let mut context = self.initial.clone();
            context.session_id = session_id;
            return Some(context);
        }
        let captured = self
            .task_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|context| context.session_id == session_id)
            .cloned();
        if captured.is_some() {
            return captured;
        }
        let current = self.current().await?;
        if current.session_id == session_id {
            Some(current)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use crate::OrchestratorConfig;
    use lingxi_core::host::OrchestratorHandle;

    #[tokio::test]
    async fn task_completion_keeps_origin_while_current_events_follow_the_new_session() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("before");
        let next_cwd = home.path().join("after");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&next_cwd).unwrap();
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                cwd.clone(),
            )
            .with_config_home(home.path().to_path_buf()),
        );
        orch.attach_owned_session_switches();
        let source = HookContextSource::new(HookContext::default());
        source.attach(&orch);
        let old = source
            .capture_task_origin("background-shell")
            .await
            .unwrap();
        assert_eq!(old.session_id, orch.current_session_id().await);
        assert_eq!(old.cwd, cwd);
        assert!(old.prompt_transcript.is_none());
        orch.clear_session().await.unwrap();
        orch.change_working_directory(next_cwd.clone(), vec![next_cwd.clone()])
            .await
            .unwrap();
        let current = source.current().await.unwrap();
        assert_eq!(current.session_id, orch.current_session_id().await);
        assert_ne!(current.session_id, old.session_id);
        assert_ne!(current.transcript_path, old.transcript_path);
        assert_eq!(current.cwd, next_cwd);
        let completion = source.task_completed("background-shell").await.unwrap();
        assert_eq!(completion.session_id, old.session_id);
        assert_eq!(completion.transcript_path, old.transcript_path);
        assert_eq!(completion.cwd, old.cwd);
        assert!(!completion.publication_guard.unwrap().is_current());
        assert!(source.task_origins.lock().unwrap().is_empty());
        let created = source.capture_task_origin("new-shell").await.unwrap();
        assert_eq!(created.session_id, current.session_id);
        assert_eq!(created.cwd, current.cwd);
        source.discard_task_origin("new-shell");
        assert!(source.task_origins.lock().unwrap().is_empty());
    }
}
