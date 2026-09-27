use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tool_api::registry::ToolRegistry;

struct InlineRuntime;
#[async_trait]
impl platform_api::RuntimeSpawner for InlineRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<platform_api::BackgroundTaskHandle, platform_api::RuntimeError> {
        tokio::spawn(task);
        Ok(platform_api::BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: 0,
        })
    }
    async fn sleep(&self, _d: std::time::Duration) {}
    async fn cancel(
        &self,
        _h: &platform_api::BackgroundTaskHandle,
    ) -> Result<(), platform_api::RuntimeError> {
        Ok(())
    }
}

struct NoopSideQuery;
#[async_trait]
impl sidequery::SideQueryClient for NoopSideQuery {
    async fn query(
        &self,
        _r: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        Ok(sidequery::SideQueryResponse {
            text: Some("{\"filenames\":[]}".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn orch_with_memdir(memdir: &std::path::Path) -> ConversationOrchestrator {
    let prefetch = memory::prefetch::MemoryPrefetch::new(
        Arc::new(memory::selector::MemorySelector::new(Arc::new(
            NoopSideQuery,
        ))),
        Arc::new(InlineRuntime),
        memory::memdir::MemdirRoots {
            user_memdir: memdir.to_path_buf(),
            session_memdir: memdir.join("session-memory"),
            team_memdir: None,
        },
    );
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
    .with_memory_prefetch(Arc::new(prefetch))
}

/// The producer only reports memdir files whose mtime is NEWER than the
/// previous scan, and the constructor seeds that floor with "now". Push it
/// back so the fixture files (written milliseconds later) always qualify.
fn reset_scan_floor(orch: &ConversationOrchestrator) {
    orch.prompt_runtime
        .last_memory_scan_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);
}

fn dream_notification(
    result: Option<&str>,
    status: &str,
) -> platform_api::task_registry::TaskNotification {
    platform_api::task_registry::TaskNotification {
        task_id: "d12345678".into(),
        task_type: "dream".into(),
        status: status.into(),
        description: "memory consolidation".into(),
        tool_use_id: None,
        output_path: None,
        exit_code: None,
        error: None,
        result: result.map(str::to_string),
        usage: None,
        killed_by: None,
        worktree_path: None,
        worktree_branch: None,
        workflow_failures: Vec::new(),
        workflow_agent_count: None,
        workflow_total_tokens: None,
        workflow_total_tool_calls: None,
        workflow_duration_ms: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_completed_dream_emits_the_three_line_reminder_once() {
    let dir = TempDir::new().unwrap();
    let orch = orch_with_memdir(dir.path());
    reset_scan_floor(&orch);
    // A memory file the model is currently holding, plus one it is not.
    let loaded = dir.path().join("loaded.md");
    let other = dir.path().join("other.md");
    std::fs::write(&loaded, "a").unwrap();
    std::fs::write(&other, "b").unwrap();
    tool_api::read_file_state::set_with_model_context(
        &orch.prompt_runtime.read_state_map,
        loaded.clone(),
        tool_api::read_file_state::ReadFileEntry {
            content: "a".into(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
        true,
    );

    orch.enqueue_memory_updates_from(&[dream_notification(Some("merged 3 notes"), "completed")]);
    let msgs = orch.memory_update_reminder_messages().await;
    assert_eq!(msgs.len(), 1);
    let text = msgs[0].text_content();
    assert!(text.starts_with("<system-reminder>\n") && text.ends_with("\n</system-reminder>"));
    assert!(
        text.contains(
            "Background memory consolidation updated your memory directory: merged 3 notes"
        ),
        "got: {text}"
    );
    assert!(text.contains("Files changed: "), "got: {text}");
    assert!(
        text.contains(&format!(
            "Your loaded copy of {} is now stale relative to disk",
            loaded.to_string_lossy()
        )),
        "only the in-context path is named stale; got: {text}"
    );
    assert!(
        !text.contains(&format!("Your loaded copy of {}", other.to_string_lossy())),
        "got: {text}"
    );
    assert!(
        text.contains(crate::prompt::memory_update::AMBIENT_CONTEXT_TRAILER),
        "got: {text}"
    );

    // Consume-once: the queue is drained.
    assert!(orch.memory_update_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn a_failed_dream_queues_nothing_and_the_result_falls_back_to_the_description() {
    let dir = TempDir::new().unwrap();
    let orch = orch_with_memdir(dir.path());

    orch.enqueue_memory_updates_from(&[dream_notification(Some("x"), "failed")]);
    assert!(
        orch.prompt_runtime
            .pending_memory_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a failed consolidation changed nothing"
    );

    orch.enqueue_memory_updates_from(&[dream_notification(Some("   "), "completed")]);
    let queued = orch
        .prompt_runtime
        .pending_memory_updates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].summary, "memory consolidation");
}

/// The BATCHED driver enqueues but never drains, so the queue is capped.
#[tokio::test]
async fn the_queue_is_capped_dropping_the_oldest() {
    let dir = TempDir::new().unwrap();
    let orch = orch_with_memdir(dir.path());
    for i in 0..(ConversationOrchestrator::MAX_PENDING_MEMORY_UPDATES + 3) {
        orch.enqueue_memory_updates_from(&[dream_notification(
            Some(&format!("run{i}")),
            "completed",
        )]);
    }
    let queued = orch
        .prompt_runtime
        .pending_memory_updates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        queued.len(),
        ConversationOrchestrator::MAX_PENDING_MEMORY_UPDATES
    );
    assert_eq!(queued[0].summary, "run3", "the OLDEST entries are dropped");
}
