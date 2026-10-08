use super::*;
use crate::prompt::memory_block::{
    InstructionFilesMode, InstructionUserEmailProvider, MemoryHierarchyProvider,
    RootInstructionContextProvider,
};
use crate::prompt::MemoryFile;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
};
use lingxi_core::host::instructions::{
    InstructionContextKey, InstructionContextProvider, InstructionRefreshReason,
    InstructionRendering, InstructionScope,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

struct MutableMemory {
    files: RwLock<Vec<MemoryFile>>,
    loads: AtomicUsize,
}

impl MutableMemory {
    fn new(path: std::path::PathBuf, body: &str) -> Arc<Self> {
        Arc::new(Self {
            files: RwLock::new(vec![MemoryFile {
                source_content: None,
                parent: None,
                path,
                body: body.into(),
                raw_content: body.into(),
                tier: memory::lingxi_md::LingxiMdTier::Project,
                is_local_override: false,
                globs: None,
                content_differs_from_disk: false,
            }]),
            loads: AtomicUsize::new(0),
        })
    }

    fn replace_body(&self, body: &str) {
        let mut files = self.files.write().unwrap();
        files[0].body = body.into();
        files[0].raw_content = body.into();
    }
}

#[async_trait]
impl MemoryHierarchyProvider for MutableMemory {
    async fn load(&self, _cwd: &std::path::Path) -> Vec<MemoryFile> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.files.read().unwrap().clone()
    }
    async fn load_conditional_rules(
        &self,
        _cwd: &std::path::Path,
        _trigger: &std::path::Path,
        _mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        Vec::new()
    }
}

struct MutableIdentity(RwLock<Option<String>>);

#[async_trait]
impl InstructionUserEmailProvider for MutableIdentity {
    async fn current_user_email(&self) -> Option<String> {
        self.0.read().unwrap().clone()
    }
}

fn orchestrator(
    config: crate::OrchestratorConfig,
    memory: Arc<dyn MemoryHierarchyProvider>,
    cwd: &std::path::Path,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        config,
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory,
        cwd.to_owned(),
    )
}

async fn body(orch: &ConversationOrchestrator) -> String {
    orch.instruction_context_snapshot()
        .await
        .eager_instructions
        .unwrap()[0]
        .content
        .clone()
}

#[tokio::test]
async fn main_prompt_consumers_share_one_eager_snapshot_across_model_steps() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "original instructions");
    let orch = orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    );
    orch.effective_system_prompt().await;
    orch.additional_context_message().await.unwrap();
    assert_eq!(body(&orch).await, "original instructions");
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
    source.replace_body("unannounced file edit");
    for _ in 0..3 {
        orch.effective_system_prompt().await;
        orch.additional_context_message().await.unwrap();
        assert_eq!(body(&orch).await, "original instructions");
    }
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
    assert_eq!(orch.prompt_runtime.instruction_cache.builds_started(), 1);
}

#[tokio::test]
async fn wc_updates_eager_files_without_replacing_a_prepared_gv_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "original instructions");
    let orch = orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    );
    orch.additional_context_message().await.unwrap();
    source.replace_body("edited instructions");
    orch.clear_instruction_files();
    assert_eq!(
        orch.instruction_file_snapshot().await.unwrap()[0].body,
        "edited instructions"
    );
    assert_eq!(
        orch.main_instruction_files().await.unwrap()[0].body,
        "original instructions"
    );
    orch.additional_context_message().await.unwrap();
    assert_eq!(body(&orch).await, "original instructions");
    assert_eq!(source.loads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn pt_rebuilds_live_identity_with_the_same_eager_file_promise() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "original instructions");
    let identity = Arc::new(MutableIdentity(RwLock::new(Some(
        "first@example.test".into(),
    ))));
    let orch = orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    )
    .with_instruction_user_email_provider(identity.clone());
    orch.additional_context_message().await.unwrap();
    source.replace_body("unannounced file edit");
    *identity.0.write().unwrap() = Some("next@example.test".into());
    orch.invalidate_instruction_context(InstructionRefreshReason::AccountChange);
    orch.additional_context_message().await.unwrap();
    let context = orch.instruction_context_snapshot().await;
    assert_eq!(
        context.eager_instructions.unwrap()[0].content,
        "original instructions"
    );
    assert!(context.user_context["userEmail"]
        .starts_with("The user's email address is next@example.test."));
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
    assert_eq!(
        orch.frozen_instruction_refresh_reason(),
        InstructionRefreshReason::AccountChange
    );
}

#[tokio::test]
async fn directory_added_emits_its_real_refresh_reason_and_reloads_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "original instructions");
    let orch = orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    );
    orch.context_announcement_messages().await;
    source.replace_body("directory event instructions");
    orch.fire_directory_added("/fixture/extra", "slash_command")
        .await;
    let rows = orch.context_announcement_messages().await;
    let payloads = orch.context_attachment_history(&rows);
    let instructions = payloads
        .iter()
        .find(|row| row["type"] == "instructions")
        .unwrap();
    assert_eq!(instructions["reason"], "directory_added");
    assert_eq!(
        instructions["files"][0]["content"],
        "directory event instructions"
    );
    assert_eq!(source.loads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_during_compaction_does_not_replace_the_querys_frozen_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "query instructions");
    let orch = orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    );
    orch.additional_context_message().await.unwrap();
    let reason = orch.frozen_instruction_refresh_reason();
    source.replace_body("next query instructions");
    orch.refresh_instruction_context(InstructionRefreshReason::Compaction);
    assert_eq!(body(&orch).await, "query instructions");
    assert_eq!(orch.frozen_instruction_refresh_reason(), reason);
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
    // A later Or producer establishes the next query's snapshot and cause.
    orch.additional_context_message().await.unwrap();
    assert_eq!(body(&orch).await, "next query instructions");
    assert_eq!(
        orch.frozen_instruction_refresh_reason(),
        InstructionRefreshReason::Compaction
    );
}

#[tokio::test]
async fn owned_clear_changes_the_typed_identity_and_retires_both_caches() {
    use lingxi_core::host::OrchestratorHandle;
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "before clear");
    let orch = Arc::new(orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    ));
    orch.attach_owned_session_switches();
    orch.additional_context_message().await.unwrap();
    let old = orch.session.lock().await.session_id;
    source.replace_body("after clear");
    orch.clear_session().await.unwrap();
    let new = orch.session.lock().await.session_id;
    assert_ne!(old, new);
    assert!(orch
        .prompt_runtime
        .instruction_cache
        .current_context(InstructionContextKey {
            session_id: old,
            agent_id: None,
        })
        .is_none());
    orch.additional_context_message().await.unwrap();
    assert_eq!(body(&orch).await, "after clear");
    assert_eq!(source.loads.load(Ordering::SeqCst), 2);
    assert_eq!(
        orch.frozen_instruction_refresh_reason(),
        InstructionRefreshReason::SessionStart
    );
}

#[tokio::test]
async fn injected_identity_none_omits_configured_fallback_and_raw_whitespace_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "instructions");
    let identity = Arc::new(MutableIdentity(RwLock::new(None)));
    let config = crate::OrchestratorConfig {
        user_email: Some("stale-config@example.test".into()),
        ..Default::default()
    };
    let orch = orchestrator(config, source, dir.path())
        .with_instruction_user_email_provider(identity.clone());
    orch.additional_context_message().await.unwrap();
    assert!(!orch
        .instruction_context_snapshot()
        .await
        .user_context
        .contains_key("userEmail"));
    *identity.0.write().unwrap() = Some("  raw@example.test \t".into());
    orch.invalidate_instruction_context(InstructionRefreshReason::AccountChange);
    orch.additional_context_message().await.unwrap();
    let context = orch.instruction_context_snapshot().await;
    assert!(context.user_context["userEmail"]
        .starts_with("The user's email address is   raw@example.test \t."));
    assert!(context.user_context["userEmail"].contains("Never send it to an unrelated service"));
}

struct GatedIdentity {
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl GatedIdentity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait]
impl InstructionUserEmailProvider for GatedIdentity {
    async fn current_user_email(&self) -> Option<String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
            Some("retired@example.test".into())
        } else {
            Some("current@example.test".into())
        }
    }
}

#[tokio::test]
async fn retired_builder_cannot_replace_a_new_sessions_frozen_context() {
    use lingxi_core::host::OrchestratorHandle;
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "retired instructions");
    let identity = GatedIdentity::new();
    let orch = Arc::new(
        orchestrator(
            crate::OrchestratorConfig::default(),
            source.clone(),
            dir.path(),
        )
        .with_instruction_user_email_provider(identity.clone()),
    );
    orch.attach_owned_session_switches();
    let old = orch.session.lock().await.session_id;
    let old_query = {
        let orch = orch.clone();
        tokio::spawn(async move { orch.additional_context_message().await })
    };
    identity.entered.notified().await;
    source.replace_body("current instructions");
    orch.clear_session().await.unwrap();
    orch.additional_context_message().await.unwrap();
    let new = orch.session.lock().await.session_id;
    assert_ne!(old, new);
    identity.release.notify_one();
    old_query.await.unwrap().unwrap();
    let context = orch.instruction_context_snapshot().await;
    assert_eq!(
        context.eager_instructions.unwrap()[0].content,
        "current instructions"
    );
    assert!(context.user_context["userEmail"].contains("current@example.test"));
    assert!(!context.user_context["userEmail"].contains("retired@example.test"));
    assert_eq!(
        *orch.prompt_runtime.instruction_context_key.lock().unwrap(),
        Some(InstructionContextKey {
            session_id: new,
            agent_id: None
        })
    );
}

#[tokio::test]
async fn inline_preparation_and_reattach_reuse_the_same_precompaction_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "prepared instructions");
    let config = crate::OrchestratorConfig {
        context_rendering: InstructionRendering::Inline,
        ..Default::default()
    };
    let orch = orchestrator(config, source.clone(), dir.path());
    // Conditional C7 discovery is a separate fresh-walk lane; this fixture
    // supplies no conditional rules and isolates the eager Gv projections.
    let prepared = orch
        .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
        .await
        .unwrap();
    let envelope = prepared
        .context_announcements
        .inline_context
        .as_ref()
        .unwrap();
    assert!(serde_json::to_string(envelope)
        .unwrap()
        .contains("prepared instructions"));
    source.replace_body("instructions for the next query");
    orch.refresh_instruction_context(InstructionRefreshReason::Compaction);
    let mut rebuilt = prepared.snapshot.clone();
    let mut turn_reminders = prepared.turn_reminders.clone();
    let mut guarded_async_hook_reminders = prepared.guarded_async_hook_reminders.clone();
    for after_compaction in [false, true] {
        orch.reattach_outgoing_context(
            &mut rebuilt,
            prepared.deferred_reminder.as_ref(),
            prepared.date_change_reminder.as_ref(),
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
            &prepared.context_announcements,
            after_compaction,
        )
        .await;
        let copies = rebuilt
            .iter()
            .filter(|message| message.id() == envelope.id())
            .collect::<Vec<_>>();
        assert_eq!(copies, vec![envelope]);
        assert!(!serde_json::to_string(&rebuilt)
            .unwrap()
            .contains("instructions for the next query"));
    }
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scalar_projection_retains_its_ingress_handle_across_a_host_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "ingress instructions");
    let orch = orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    );
    let (key, load) = orch.main_instruction_load().await;
    load.get().await.unwrap();
    source.replace_body("instructions for a later ingress");
    orch.refresh_instruction_context(InstructionRefreshReason::PolicyRefresh);
    let original = orch
        .additional_context_message_from_load(key, &load)
        .await
        .unwrap()
        .unwrap();
    let original = serde_json::to_string(&original).unwrap();
    assert!(original.contains("ingress instructions"));
    assert!(!original.contains("instructions for a later ingress"));
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
    let later = orch.additional_context_message().await.unwrap();
    assert!(serde_json::to_string(&later)
        .unwrap()
        .contains("instructions for a later ingress"));
    assert_eq!(source.loads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn children_project_the_current_roots_same_full_context_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(
        dir.path().join("instructions.md"),
        "root project instructions",
    );
    source.files.write().unwrap().push(MemoryFile {
        source_content: None,
        parent: None,
        path: dir.path().join("managed/instructions.md"),
        body: "managed instructions".into(),
        raw_content: "managed instructions".into(),
        tier: memory::lingxi_md::LingxiMdTier::Managed,
        is_local_override: false,
        globs: None,
        content_differs_from_disk: false,
    });
    let root = Arc::new(orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    ));
    let provider = RootInstructionContextProvider::new();
    provider.bind(&root).unwrap();
    let caller_cwd = dir.path().join("unrelated-child-cwd");
    let full = provider
        .load(&caller_cwd, InstructionScope::Full)
        .await
        .unwrap();
    let managed = provider
        .load(&caller_cwd, InstructionScope::ManagedOnly)
        .await
        .unwrap();
    assert!(full.user_context["instructions"].contains("root project instructions"));
    assert!(managed.user_context["instructions"].contains("managed instructions"));
    assert!(!managed.user_context["instructions"].contains("root project instructions"));
    assert_eq!(
        managed.user_context["currentDate"],
        full.user_context["currentDate"]
    );
    assert_eq!(
        full.instructions_root,
        Some(memory::lingxi_md::agents::identity(dir.path()))
    );
    assert_eq!(managed.eager_instructions.unwrap().len(), 1);
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
    source.replace_body("current root instructions");
    root.refresh_instruction_context(InstructionRefreshReason::DirectoryAdded);
    let next = provider
        .load(&caller_cwd, InstructionScope::Full)
        .await
        .unwrap();
    assert!(next.user_context["instructions"].contains("current root instructions"));
    assert_eq!(source.loads.load(Ordering::SeqCst), 2);
    assert!(provider.bind(&root).is_err());
}

#[tokio::test]
async fn pending_child_reader_does_not_keep_the_root_alive() {
    let dir = tempfile::tempdir().unwrap();
    let source = MutableMemory::new(dir.path().join("instructions.md"), "instructions");
    let identity = GatedIdentity::new();
    let root = Arc::new(
        orchestrator(crate::OrchestratorConfig::default(), source, dir.path())
            .with_instruction_user_email_provider(identity.clone()),
    );
    let weak = Arc::downgrade(&root);
    let provider = Arc::new(RootInstructionContextProvider::new());
    provider.bind(&root).unwrap();
    let child = {
        let provider = provider.clone();
        let cwd = dir.path().to_owned();
        tokio::spawn(async move { provider.load(&cwd, InstructionScope::Full).await })
    };
    identity.entered.notified().await;
    drop(root);
    assert!(weak.upgrade().is_none());
    assert!(!provider.is_bound());
    identity.release.notify_one();
    assert!(
        child.await.unwrap().unwrap().user_context["userEmail"].contains("retired@example.test")
    );
}

struct FailedManagedMemory(AtomicUsize);

#[async_trait]
impl MemoryHierarchyProvider for FailedManagedMemory {
    async fn load(&self, _cwd: &std::path::Path) -> Vec<MemoryFile> {
        panic!("managed-only context must use the result-bearing file producer");
    }

    async fn load_managed(&self, _cwd: &std::path::Path) -> Result<Vec<MemoryFile>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("native context acquisition failed".into())
    }
    async fn load_conditional_rules(
        &self,
        _cwd: &std::path::Path,
        _trigger: &std::path::Path,
        _mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        Vec::new()
    }

    fn instruction_files_mode(&self) -> InstructionFilesMode {
        InstructionFilesMode::ManagedOnly
    }
}

#[tokio::test]
async fn rejected_managed_context_stays_sticky_and_prevents_model_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(FailedManagedMemory(AtomicUsize::new(0)));
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let root = Arc::new(ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory.clone(),
        dir.path().to_owned(),
    ));
    let provider = RootInstructionContextProvider::new();
    provider.bind(&root).unwrap();
    assert_eq!(
        provider
            .load(dir.path(), InstructionScope::Full)
            .await
            .unwrap_err(),
        "native context acquisition failed"
    );
    for _ in 0..2 {
        let error = root.run_turn("hello").await.unwrap_err();
        assert!(error
            .to_string()
            .contains("native context acquisition failed"));
    }
    assert!(api.captured_msgs().await.is_empty());
    assert_eq!(memory.0.load(Ordering::SeqCst), 1);
    assert_eq!(root.prompt_runtime.instruction_cache.builds_started(), 1);
}

struct ReservationGateMemory {
    cwd: std::path::PathBuf,
    stop_mode_once: std::sync::atomic::AtomicBool,
    mode_entered: tokio::sync::Notify,
    mode_released: std::sync::Mutex<bool>,
    release_mode: std::sync::Condvar,
    loads: AtomicUsize,
    file_entered: tokio::sync::Notify,
    release_file: tokio::sync::Notify,
}

#[async_trait]
impl MemoryHierarchyProvider for ReservationGateMemory {
    async fn load(&self, _cwd: &std::path::Path) -> Vec<MemoryFile> {
        let first = self.loads.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            self.file_entered.notify_one();
            self.release_file.notified().await;
        }
        let body = if first {
            "original waiter"
        } else {
            "current session"
        };
        vec![MemoryFile {
            source_content: None,
            parent: None,
            path: self.cwd.join("instructions.md"),
            body: body.into(),
            raw_content: body.into(),
            tier: memory::lingxi_md::LingxiMdTier::Project,
            is_local_override: false,
            globs: None,
            content_differs_from_disk: false,
        }]
    }

    async fn load_conditional_rules(
        &self,
        _cwd: &std::path::Path,
        _trigger: &std::path::Path,
        _mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        Vec::new()
    }

    fn instruction_files_mode(&self) -> InstructionFilesMode {
        if self.stop_mode_once.swap(false, Ordering::SeqCst) {
            self.mode_entered.notify_one();
            let mut released = self.mode_released.lock().unwrap();
            while !*released {
                released = self.release_mode.wait(released).unwrap();
            }
        }
        InstructionFilesMode::LingxiMd
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn root_reservation_retains_identity_through_clear_and_preserves_old_waiters() {
    use lingxi_core::host::OrchestratorHandle;
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(ReservationGateMemory {
        cwd: dir.path().to_owned(),
        stop_mode_once: std::sync::atomic::AtomicBool::new(true),
        mode_entered: tokio::sync::Notify::new(),
        mode_released: std::sync::Mutex::new(false),
        release_mode: std::sync::Condvar::new(),
        loads: AtomicUsize::new(0),
        file_entered: tokio::sync::Notify::new(),
        release_file: tokio::sync::Notify::new(),
    });
    let root = Arc::new(orchestrator(
        crate::OrchestratorConfig::default(),
        source.clone(),
        dir.path(),
    ));
    root.attach_owned_session_switches();
    let old_key = InstructionContextKey {
        session_id: root.session.lock().await.session_id,
        agent_id: None,
    };
    let provider = Arc::new(RootInstructionContextProvider::new());
    provider.bind(&root).unwrap();
    let old_reader = {
        let provider = provider.clone();
        let cwd = dir.path().to_owned();
        tokio::spawn(async move { provider.load(&cwd, InstructionScope::Full).await })
    };
    source.mode_entered.notified().await;
    // This source gate is after ID capture and before cache reservation. A
    // real clear must share that identity boundary instead of publishing in
    // between those two operations. Release the blocking gate before asserting
    // so a failing regression cannot strand a Tokio worker during shutdown.
    let identity_was_available = root.session.try_lock().is_ok();
    let clear = {
        let root = root.clone();
        tokio::spawn(async move { root.clear_session().await })
    };
    *source.mode_released.lock().unwrap() = true;
    source.release_mode.notify_one();
    assert!(
        !identity_was_available,
        "ID capture must retain authority until reservation"
    );
    source.file_entered.notified().await;
    clear.await.unwrap().unwrap();
    assert!(root
        .prompt_runtime
        .instruction_cache
        .current_context(old_key)
        .is_none());
    let current = provider
        .load(dir.path(), InstructionScope::Full)
        .await
        .unwrap();
    assert!(current.user_context["instructions"].contains("current session"));
    source.release_file.notify_one();
    let retired = old_reader.await.unwrap().unwrap();
    assert!(retired.user_context["instructions"].contains("original waiter"));
    assert!(root
        .prompt_runtime
        .instruction_cache
        .current_context(old_key)
        .is_none());
    assert_eq!(source.loads.load(Ordering::SeqCst), 2);
}
