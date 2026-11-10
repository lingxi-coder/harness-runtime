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
impl lingxi_core::host::RuntimeSpawner for InlineRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        tokio::spawn(task);
        Ok(lingxi_core::host::BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: 0,
        })
    }
    async fn sleep(&self, _d: std::time::Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
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

#[tokio::test]
async fn prompt_static_memory_directory_reaches_actual_requests_with_an_empty_memdir() {
    use lingxi_core::host::instructions::InstructionRendering;
    for rendering in [
        InstructionRendering::Inline,
        InstructionRendering::Announced,
    ] {
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "gpt-5.5",
        ] {
            let directory = TempDir::new().unwrap();
            let memdir = directory.path().join("empty-memdir");
            std::fs::create_dir(&memdir).unwrap();
            let mut response = crate::test_support::mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            );
            response.model = model.into();
            let api = Arc::new(MockApiClient::new(vec![response]));
            let mut orch = orch_with_memdir(&memdir);
            orch.api = api.clone();
            orch.config.model = model.into();
            orch.session.lock().await.model = model.into();
            orch.config.exclude_dynamic_system_prompt_sections = true;
            orch.config.context_rendering = rendering;
            orch.run_turn("save future preferences in memory")
                .await
                .unwrap();
            let requests = api.captured_requests().await;
            let crate::conversation::OrchestratorApiRequest::Main(request) = &requests[0] else {
                panic!("main request")
            };
            let system = request.system.as_ref().unwrap().display_text();
            let messages = request
                .messages
                .iter()
                .map(|message| message.text_content())
                .collect::<Vec<_>>()
                .join("\n");
            let directory_text = memdir.to_string_lossy();
            if model.starts_with("claude-") {
                assert!(
                    !system.contains(directory_text.as_ref()),
                    "static body: {model}"
                );
                assert_eq!(
                    messages.matches(directory_text.as_ref()).count(),
                    1,
                    "{rendering:?} {model}: {messages}"
                );
                if model != "claude-sonnet-4-6" {
                    assert!(
                        messages.contains("# Memory\n"),
                        "Lean protocol must accompany the directory: {messages}"
                    );
                    assert!(
                        messages.contains("persistent file-based memory"),
                        "{messages}"
                    );
                }
            } else {
                assert_eq!(system.matches(directory_text.as_ref()).count(), 1);
                assert!(
                    !messages.contains(directory_text.as_ref()),
                    "generic memory remains in its body"
                );
            }
            assert_eq!(
                std::fs::read_dir(&memdir).unwrap().count(),
                0,
                "no surfaced memory file supplied the path"
            );
        }
    }
}

#[tokio::test]
async fn prompt_static_memory_directory_respects_complete_system_override() {
    let directory = TempDir::new().unwrap();
    let orch = orch_with_memdir(directory.path());
    let mut orch = orch;
    orch.config.exclude_dynamic_system_prompt_sections = true;
    orch.config.system_prompt_override = Some("CUSTOM SYSTEM".into());
    let context = orch
        .additional_context_message()
        .await
        .unwrap()
        .unwrap()
        .text_content();
    assert!(!context.contains(directory.path().to_string_lossy().as_ref()));
    assert!(!context.contains("# Memory\n"));
}

#[tokio::test]
async fn prompt_dynamic_memory_survives_old_announcements_model_switches_and_plugin_keys() {
    let directory = TempDir::new().unwrap();
    let memdir = directory.path().join("empty-memdir");
    std::fs::create_dir(&memdir).unwrap();
    let module = directory.path().join("memory-context.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('prompt.context', ($, e, next) => next({ ...e,
        blocks: [...e.blocks, { name: 'Memory', text: 'PROJECT MEMORY RULES' }]
      }));
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "memory-context",
        directory.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut hooks = hooks::HookRegistry::new();
    hooks.set_mod_host(host);
    let models = [
        "gpt-5.5",
        "claude-opus-5",
        "claude-sonnet-5-5",
        "claude-sonnet-4-6",
        "gpt-5.5",
    ];
    let responses = models
        .iter()
        .map(|model| {
            let mut response = crate::test_support::mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            );
            response.model = (*model).into();
            response
        })
        .collect();
    let api = Arc::new(MockApiClient::new(responses));
    let mut orch =
        orch_with_memdir(&memdir).with_hook_registry(Arc::new(tokio::sync::RwLock::new(hooks)));
    orch.api = api.clone();
    orch.config.exclude_dynamic_system_prompt_sections = true;
    for (index, model) in models.iter().enumerate() {
        orch.session.lock().await.model = (*model).into();
        orch.run_turn("keep using memory").await.unwrap();
        let requests = api.captured_requests().await;
        let crate::conversation::OrchestratorApiRequest::Main(request) = &requests[index] else {
            panic!("main request")
        };
        assert_eq!(request.model, *model);
        let text = request
            .messages
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text.matches("PROJECT MEMORY RULES").count(),
            1,
            "plugin Memory key: {model}: {text}"
        );
        assert_eq!(
            text.matches(memdir.to_string_lossy().as_ref()).count(),
            usize::from(model.starts_with("claude-")),
            "{model}: {text}"
        );
        let dynamic_id = request
            .messages
            .iter()
            .find(|message| {
                matches!(message,
            lingxi_core::types::ConversationMessage::User { origin: Some(origin), .. }
            if origin.string_at("/type") == Some("dynamic_memory_context"))
            })
            .map(|message| message.id());
        let mut retry = request.messages.clone();
        orch.project_dynamic_memory_context(&mut retry).await;
        assert_eq!(
            retry
                .iter()
                .find(|message| matches!(message,
            lingxi_core::types::ConversationMessage::User { origin: Some(origin), .. }
            if origin.string_at("/type") == Some("dynamic_memory_context")))
                .map(|message| message.id()),
            dynamic_id,
            "a retry reuses the exact dynamic message"
        );
        assert!(
            !orch
                .session
                .lock()
                .await
                .history
                .iter()
                .any(|message| matches!(message,
            lingxi_core::types::ConversationMessage::User { origin: Some(origin), .. }
            if origin.string_at("/type") == Some("dynamic_memory_context"))),
            "outgoing context is not persisted"
        );
    }
}

#[tokio::test]
async fn prompt_teammate_host_context_uses_live_cwd_and_actual_model() {
    let directory = TempDir::new().unwrap();
    let initial_cwd = directory.path().join("initial");
    let next_cwd = directory.path().join("next");
    std::fs::create_dir(&initial_cwd).unwrap();
    std::fs::create_dir(&next_cwd).unwrap();
    let cwd = tool_api::SessionCwd::new(initial_cwd.clone(), Vec::new());
    let mut orch = orch_with_memdir(&directory.path().join("memory")).with_session_cwd(cwd.clone());
    orch.config.exclude_dynamic_system_prompt_sections = true;
    for model in ["claude-sonnet-4-6", "claude-opus-5", "gpt-5.5", "kimi-k2.5"] {
        let target = agent::prompt_policy::resolve_target(
            None,
            model,
            None,
            lingxi_core::host::session_context::ConversationMode::Code,
        )
        .unwrap();
        let before = orch
            .assemble_subagent_prompt_context_for_target_preview(&target)
            .await
            .unwrap();
        assert!(before.additional_context[0].contains(initial_cwd.to_string_lossy().as_ref()));
        cwd.swap(next_cwd.clone(), Vec::new());
        let rendered = orch
            .assemble_subagent_prompt_context_for_target_preview(&target)
            .await
            .unwrap();
        let text = rendered.additional_context.join("\n\n");
        assert!(text.contains(next_cwd.to_string_lossy().as_ref()), "{text}");
        assert!(
            !text.contains(initial_cwd.to_string_lossy().as_ref()),
            "{text}"
        );
        for field in ["Platform:", "Shell:", model, "Today's date is "] {
            assert!(text.contains(field), "{model}: {text}");
        }
        assert!(
            rendered.system_suffix.is_none(),
            "host body already owns the native budget tail"
        );
        cwd.swap(initial_cwd.clone(), Vec::new());
    }
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
) -> lingxi_core::host::task_registry::TaskNotification {
    lingxi_core::host::task_registry::TaskNotification {
        task_id: "d12345678".into(),
        task_type: "dream".into(),
        status: status.into(),
        description: "memory consolidation".into(),
        tool_use_id: None,
        output_path: None,
        exit_code: None,
        error: None,
        result: result.map(|result| result.to_owned().into()),
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
    assert_eq!(
        queued[0].summary.string_at(""),
        Some("memory consolidation")
    );
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
    assert_eq!(
        queued[0].summary.string_at(""),
        Some("run3"),
        "the OLDEST entries are dropped"
    );
}
