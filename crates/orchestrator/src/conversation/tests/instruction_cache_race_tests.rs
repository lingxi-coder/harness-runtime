//! Root-provider ingress races against real owned clear/resume activation.

use super::*;
use crate::prompt::memory_block::{MemoryHierarchyProvider, RootInstructionContextProvider};
use crate::prompt::MemoryFile;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
};
use lingxi_core::host::instructions::{InstructionContextProvider, InstructionScope};
use lingxi_core::host::{OrchestratorHandle, PromptSnapshot, ResumeRuntimeSnapshot};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

struct MutableMemory {
    file: RwLock<MemoryFile>,
    loads: AtomicUsize,
}

impl MutableMemory {
    fn new(path: PathBuf, body: &str) -> Arc<Self> {
        Arc::new(Self {
            file: RwLock::new(MemoryFile {
                source_content: None,
                parent: None,
                path,
                body: body.to_owned(),
                raw_content: body.to_owned(),
                tier: memory::lingxi_md::LingxiMdTier::Project,
                is_local_override: false,
                globs: None,
                content_differs_from_disk: false,
            }),
            loads: AtomicUsize::new(0),
        })
    }

    fn replace_body(&self, body: &str) {
        let mut file = self.file.write().unwrap();
        file.body = body.to_owned();
        file.raw_content = body.to_owned();
    }
}

#[async_trait]
impl MemoryHierarchyProvider for MutableMemory {
    async fn load(&self, _cwd: &std::path::Path) -> Vec<MemoryFile> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        vec![self.file.read().unwrap().clone()]
    }
    async fn load_conditional_rules(
        &self,
        _cwd: &std::path::Path,
        _trigger: &std::path::Path,
        _mode: crate::prompt::memory_block::InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        Vec::new()
    }
}

/// The switch waits for the held main-reports lock only after ordinary async
/// reset has cleared this real production marker. The timeout is a deadlock
/// guard; no result depends on elapsed time or a scheduling-speed threshold.
async fn wait_for_async_reset(root: &ConversationOrchestrator) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if root.prompt_runtime.prompt_snapshot.lock().await.is_none() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned reset reaches the prompt marker before reports activation");
}

async fn assert_root_context_activation_race(resume_same_id: bool) {
    let dir = tempfile::tempdir().unwrap();
    let memory = MutableMemory::new(dir.path().join("instructions.md"), "before reset");
    let root = Arc::new(ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory.clone(),
        dir.path().to_owned(),
    ));
    root.attach_owned_session_switches();
    let provider = RootInstructionContextProvider::new();
    provider.bind(&root).unwrap();
    let caller_cwd = dir.path().join("child-path-does-not-define-root");
    let initial_id = root.session.lock().await.session_id;
    let initial = provider
        .load(&caller_cwd, InstructionScope::Full)
        .await
        .unwrap();
    assert_eq!(
        initial.eager_instructions.as_ref().unwrap()[0].content,
        "before reset"
    );
    assert!(initial.user_context["instructions"].contains("before reset"));
    assert_eq!(memory.loads.load(Ordering::SeqCst), 1);

    *root.prompt_runtime.prompt_snapshot.lock().await = Some(PromptSnapshot {
        system_prompt: vec!["owned-reset-progress-marker".to_owned()],
        ..Default::default()
    });
    // Preparation and async reset have no dependency on this fixture's reports
    // state. Holding it pauses the actual publication transaction before the
    // session identity, root caches and instruction cursor change together.
    let reports_activation_gate = root.main_reports.state.lock().await;
    memory.replace_body("unpublished reset instructions");
    let resumed_history = vec![ConversationMessage::user(
        MessageId::new(),
        "history belonging to the resumed generation".to_owned(),
    )];
    let switch_history = resumed_history.clone();
    let switching = {
        let root = root.clone();
        tokio::spawn(async move {
            if resume_same_id {
                root.resume_session(
                    initial_id,
                    switch_history,
                    None,
                    None,
                    ResumeRuntimeSnapshot::default(),
                )
                .await
            } else {
                root.clear_session().await
            }
        })
    };
    wait_for_async_reset(&root).await;
    assert_eq!(root.session.lock().await.session_id, initial_id);
    assert!(!switching.is_finished());

    // This child ingress occurs after ordinary reset but before publication.
    // It must still observe the old memo. The defective early retirement
    // repopulates old-id Gv here with the intermediate file contents.
    let during_reset = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.load(&caller_cwd, InstructionScope::Full),
    )
    .await
    .expect("root ingress does not wait for reports activation")
    .unwrap();
    assert_eq!(
        during_reset.eager_instructions.as_ref().unwrap()[0].content,
        "before reset"
    );
    assert_eq!(during_reset.user_context, initial.user_context);
    assert_eq!(memory.loads.load(Ordering::SeqCst), 1);

    memory.replace_body("published session instructions");
    drop(reports_activation_gate);
    tokio::time::timeout(std::time::Duration::from_secs(5), switching)
        .await
        .expect("released owned switch reaches activation")
        .unwrap()
        .unwrap();
    let current_id = root.session.lock().await.session_id;
    if resume_same_id {
        assert_eq!(current_id, initial_id);
        assert_eq!(root.session.lock().await.history, resumed_history);
    } else {
        assert_ne!(current_id, initial_id);
        assert!(root.session.lock().await.history.is_empty());
    }
    let after_activation = provider
        .load(&caller_cwd, InstructionScope::Full)
        .await
        .unwrap();
    assert_eq!(
        after_activation.eager_instructions.as_ref().unwrap()[0].content,
        "published session instructions"
    );
    assert!(
        after_activation.user_context["instructions"].contains("published session instructions")
    );
    assert!(
        !after_activation.user_context["instructions"].contains("unpublished reset instructions")
    );
    assert_eq!(memory.loads.load(Ordering::SeqCst), 2);
    assert_eq!(root.prompt_runtime.instruction_cache.builds_started(), 2);
}

#[tokio::test]
async fn root_provider_clear_publishes_new_identity_and_context_in_one_activation() {
    assert_root_context_activation_race(false).await;
}

#[tokio::test]
async fn root_provider_same_id_resume_retires_the_previous_context_generation_at_activation() {
    assert_root_context_activation_race(true).await;
}
