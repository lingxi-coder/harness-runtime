use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::instructions::InstructionRendering;
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;

fn orchestrator(root: &std::path::Path) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
}

fn mode_source(id: uuid::Uuid, case: &str) -> String {
    let user = uuid::Uuid::new_v4();
    let snapshot = |text: &str| serde_json::json!({
        "type":"attachment", "uuid":uuid::Uuid::new_v4(), "sessionId":id,
        "parentUuid":user, "attachment": {"type":"prompt_snapshot",
            "systemPrompt":[text], "tools":[], "contextRendering":"inline"}
    });
    let mut rows = vec![serde_json::json!({
        "type":"user", "uuid":user, "sessionId":id, "parentUuid":null,
        "message":{"role":"user", "content":"hello"}
    })];
    if case != "missing" {
        rows.push(snapshot("old prefix"));
    }
    if case != "legacy" {
        rows.push(serde_json::json!({"type":"session-mode", "sessionId":id,
            "sessionMode":"chat", "contextRevision":1, "profileEpoch":1,
            "admittedMode":"chat", "admittedEpoch":1, "pending":false}));
        if case == "current" {
            rows.push(snapshot("current prefix"));
        }
        // A queued change must retain the snapshot of the admitted epoch.
        rows.push(serde_json::json!({"type":"session-mode", "sessionId":id,
            "sessionMode":"code", "contextRevision":2, "profileEpoch":2,
            "admittedMode":"chat", "admittedEpoch":1, "pending":true}));
    }
    rows.iter().map(|row| format!("{row}\n")).collect()
}

#[test]
fn legacy_snapshot_uuid_case_is_normalized() {
    let id = uuid::Uuid::new_v4();
    let source = mode_source(id, "legacy").replace(&id.to_string(), &id.to_string().to_uppercase());
    let (snapshot, rendering) = crate::resume::prompt_state_for_epoch_from_source(&source, &id.to_string(), 0);
    assert_eq!(snapshot.unwrap().system_prompt, vec!["old prefix"]);
    assert_eq!(rendering, Some(InstructionRendering::Inline));
}

#[tokio::test]
async fn unshared_public_profile_selection_fails_before_mutating_state() {
    let root = tempfile::tempdir().unwrap();
    let orch = orchestrator(root.path());
    assert!(orch.set_conversation_mode(ConversationMode::Chat).await.is_err());
    let state = orch.conversation_mode().await;
    assert_eq!((state.active, state.selected, state.revision, state.pending),
        (ConversationMode::Code, ConversationMode::Code, 0, false));
}

async fn assert_prompt_state(orch: &ConversationOrchestrator, case: &str) {
    let expected = match case {
        "legacy" => Some("old prefix"),
        "current" => Some("current prefix"),
        _ => None,
    };
    let snapshot = orch.prompt_runtime.prompt_snapshot.lock().await.clone();
    assert_eq!(snapshot.as_ref().map(|s| s.system_prompt[0].as_str()), expected);
    assert!(orch.prompt_snapshot_resume());
    assert_eq!(
        *orch.prompt_runtime.context_rendering_hint.lock().await,
        expected.map(|_| InstructionRendering::Inline)
    );
    if expected.is_some() {
        assert_eq!(orch.current_context_rendering().await, InstructionRendering::Inline);
    } else {
        let rendered = orch.provider_system_prompt().await.unwrap();
        orch.record_prompt_snapshot_if_needed(Some(&rendered), &[]).await;
        assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
    }
}

#[tokio::test]
async fn captured_cold_profile_preserves_only_its_epoch_prompt_state() {
    let root = tempfile::tempdir().unwrap();
    for case in ["legacy", "current", "wrong_epoch", "missing"] {
        let id = lingxi_core::types::SessionId::new();
        let source = mode_source(id.as_uuid(), case);
        let loaded = session::jsonl::route_lines(&source);
        let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &id.as_uuid().to_string());
        let profile = session::JsonlWriter::session_mode_transition_from_source(&source, &id.as_uuid().to_string());
        let orch = orchestrator(root.path()).with_session_id(id);
        orch.restore_captured_session_history(
            id.as_uuid(), &messages, &loaded.messages_in_order, false,
            permission::PermissionMode::Default, profile, &source,
        ).await.unwrap();
        assert_prompt_state(&orch, case).await;
    }
}

#[tokio::test]
async fn hot_resume_profile_preserves_only_its_epoch_prompt_state() {
    let root = tempfile::tempdir().unwrap();
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()));
    for case in ["legacy", "current", "wrong_epoch", "missing"] {
        let id = lingxi_core::types::SessionId::new();
        let source = mode_source(id.as_uuid(), case);
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        std::fs::write(&path, &source).unwrap();
        let replayed = crate::resume::replay_captured_session(
            path.clone(), source.clone(), session::jsonl::route_lines(&source), id.as_uuid(),
        ).unwrap();
        let initial = root.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let writer = Arc::new(session::JsonlWriter::new(initial, fs.clone()));
        let orch = ConversationOrchestrator::into_shared(
            orchestrator(root.path()).with_jsonl_writer(writer),
        );
        orch.attach_owned_session_switches();
        orch.resume_session(
            id, replayed.state.history.clone(), replayed.last_message_uuid.map(|v| v.to_string()),
            None, replayed.handle_runtime_snapshot(),
        ).await.unwrap();
        assert_prompt_state(&orch, case).await;
    }
}

#[tokio::test]
async fn public_with_resume_restores_chat_epoch_and_current_skill_from_captured_metadata() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let sid = uuid::Uuid::new_v4();
    let first = uuid::Uuid::new_v4();
    let old = uuid::Uuid::new_v4();
    let current = uuid::Uuid::new_v4();
    let tail = uuid::Uuid::new_v4();
    let rows = [
        serde_json::json!({"type":"user", "uuid":first, "sessionId":sid, "parentUuid":null,
            "message":{"role":"user", "content":"question"}}),
        serde_json::json!({"type":"user", "uuid":old, "sessionId":sid, "parentUuid":first, "isMeta":true,
            "origin":{"type":"skill_activation", "context_epoch":0, "skill_name":"old", "skill_path":"/skills/old"},
            "message":{"role":"user", "content":"old skill"}}),
        serde_json::json!({"type":"session-mode", "sessionId":sid, "sessionMode":"chat", "contextRevision":1,
            "profileEpoch":1, "admittedMode":"chat", "admittedEpoch":1, "pending":false}),
        serde_json::json!({"type":"user", "uuid":current, "sessionId":sid, "parentUuid":old, "isMeta":true,
            "origin":{"type":"skill_activation", "context_epoch":1, "skill_name":"current", "skill_path":"/skills/current"},
            "message":{"role":"user", "content":"current skill"}}),
        serde_json::json!({"type":"user", "uuid":tail, "sessionId":sid, "parentUuid":current,
            "message":{"role":"user", "content":"continue"}}),
        serde_json::json!({"type":"attachment", "uuid":uuid::Uuid::new_v4(), "sessionId":sid, "parentUuid":tail,
            "attachment":{"type":"prompt_snapshot", "systemPrompt":["chat prefix"], "tools":[], "contextRendering":"inline"}}),
    ];
    let path = session::jsonl::session_path(&home, &cwd.to_string_lossy(), &sid.to_string());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, rows.iter().map(|row| format!("{row}\n")).collect::<String>()).unwrap();
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()));
    let orch = ConversationOrchestrator::with_resume(
        crate::OrchestratorConfig::default(), sid, home, cwd.to_string_lossy().into_owned(), fs,
        Arc::new(MockApiClient::new(Vec::new())), Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(), Arc::new(NoOpPermissionGate), Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()), cwd, None,
    ).await.unwrap();
    let profile = orch.conversation_mode().await;
    assert_eq!((profile.active, profile.selected, profile.revision, profile.pending),
        (ConversationMode::Chat, ConversationMode::Chat, 1, false));
    assert_eq!(orch.profile_epoch().await, 1);
    let session = orch.session.lock().await;
    let history = session.model_context_history();
    assert!(!history.iter().any(|row| row.id().as_uuid() == old));
    assert!(history.iter().any(|row| row.id().as_uuid() == current));
    drop(session);
    assert_eq!(orch.prompt_runtime.prompt_snapshot.lock().await.as_ref().unwrap().system_prompt, vec!["chat prefix"]);
    assert_eq!(orch.current_context_rendering().await, InstructionRendering::Inline);
    assert!(orch.prompt_snapshot_resume());
}

fn durable_orchestrator(root: &std::path::Path) -> (
    Arc<ConversationOrchestrator>, Arc<session::jsonl::DurableTranscriptWriter>, std::path::PathBuf,
) {
    let id = lingxi_core::types::SessionId::new();
    let path = root.join(format!("{}.jsonl", id.as_uuid()));
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(platform_posix::fs::PosixFileSystem::new(root.to_path_buf()));
    let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(root).unwrap());
    let writer = session::JsonlWriter::new(path.clone(), fs).with_durable_lock(durable.clone());
    writer.activate_session_target(id, path.clone(), root.to_path_buf()).unwrap();
    let orch = ConversationOrchestrator::into_shared(crate::test_support::with_scripted_compactor(
        orchestrator(root).with_session_id(id).with_jsonl_writer(Arc::new(writer)),
        "retained task context",
    ));
    (orch, durable, path)
}

async fn assert_retry_and_cold(orch: &ConversationOrchestrator, path: &std::path::Path) {
    let selected = tokio::time::timeout(std::time::Duration::from_secs(5),
        orch.set_conversation_mode(ConversationMode::Chat)).await.unwrap().unwrap();
    assert_eq!((selected.active, selected.selected, selected.revision, selected.pending),
        (ConversationMode::Chat, ConversationMode::Chat, 1, false));
    let content = std::fs::read_to_string(path).unwrap();
    let persisted = session::JsonlWriter::session_mode_transition_from_source(
        &content, &selected.session_id,
    ).unwrap();
    assert_eq!((persisted.active, persisted.selected, persisted.revision, persisted.pending),
        (ConversationMode::Chat, ConversationMode::Chat, 1, false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_profile_caller_during_blocked_durable_append_still_settles() {
    let root = tempfile::tempdir().unwrap();
    let (orch, durable, path) = durable_orchestrator(root.path());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocking = tokio::task::spawn_blocking(move || durable.with_transaction(|_| {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(())
    }));
    entered_rx.await.unwrap();
    let owner = orch.clone();
    let caller = tokio::spawn(async move { owner.set_conversation_mode(ConversationMode::Chat).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while orch.conversation_transition.try_lock().is_ok() { tokio::task::yield_now().await; }
    }).await.unwrap();
    caller.abort();
    let _ = caller.await;
    release_tx.send(()).unwrap();
    blocking.await.unwrap().unwrap();
    assert_retry_and_cold(&orch, &path).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_a_detached_profile_selection_before_closing_session_state() {
    let root = tempfile::tempdir().unwrap();
    let (orch, durable, path) = durable_orchestrator(root.path());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked = tokio::task::spawn_blocking(move || durable.with_transaction(|_| {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(())
    }));
    entered_rx.await.unwrap();
    let owner = orch.clone();
    let caller = tokio::spawn(async move { owner.set_conversation_mode(ConversationMode::Chat).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while orch.conversation_transition.try_lock().is_ok() { tokio::task::yield_now().await; }
    }).await.unwrap();
    caller.abort();
    let _ = caller.await;
    let drain_owner = orch.clone();
    let drain = tokio::spawn(async move { drain_owner.close_and_drain_session_switches().await });
    tokio::task::yield_now().await;
    assert!(!drain.is_finished(), "shutdown cannot close session state while selection is blocked");
    release_tx.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(5), drain).await.unwrap().unwrap().is_empty());
    let state = orch.conversation_mode().await;
    assert_eq!((state.active, state.selected, state.revision, state.pending),
        (ConversationMode::Chat, ConversationMode::Chat, 1, false));
    let cold = session::JsonlWriter::session_mode_transition_from_source(
        &std::fs::read_to_string(path).unwrap(), &state.session_id,
    ).unwrap();
    assert_eq!((cold.active, cold.selected, cold.revision, cold.pending),
        (ConversationMode::Chat, ConversationMode::Chat, 1, false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_profile_caller_during_projection_reset_still_settles() {
    let root = tempfile::tempdir().unwrap();
    let (orch, _, path) = durable_orchestrator(root.path());
    let blocked = orch.prompt_runtime.prompt_snapshot.lock().await;
    let owner = orch.clone();
    let caller = tokio::spawn(async move { owner.set_conversation_mode(ConversationMode::Chat).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(&path).is_ok_and(|source| source.contains("\"sessionMode\":\"chat\"")) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    caller.abort();
    let _ = caller.await;
    drop(blocked);
    assert_retry_and_cold(&orch, &path).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_compaction_admission_keeps_turn_gate_until_durable_and_live_agree() {
    let root = tempfile::tempdir().unwrap();
    let (orch, durable, path) = durable_orchestrator(root.path());
    let turn = orch.turn_gate.lock().await;
    assert!(orch.set_conversation_mode(ConversationMode::Chat).await.unwrap().pending);
    drop(turn);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked = tokio::task::spawn_blocking(move || durable.with_transaction(|_| {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(())
    }));
    entered_rx.await.unwrap();
    let owner = orch.clone();
    let compact = tokio::spawn(async move { owner.force_compact().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while orch.conversation_transition.try_lock().is_ok() { tokio::task::yield_now().await; }
    }).await.unwrap();
    compact.abort();
    let _ = compact.await;
    assert!(orch.turn_gate.try_lock().is_err(), "admission retains the real turn gate after caller abort");
    let next_owner = orch.clone();
    let selection = tokio::spawn(async move { next_owner.set_conversation_mode(ConversationMode::Code).await });
    tokio::task::yield_now().await;
    assert!(!selection.is_finished(), "a later selection cannot race the blocked admission");
    release_tx.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    let selected = tokio::time::timeout(std::time::Duration::from_secs(5), selection).await.unwrap().unwrap().unwrap();
    assert_eq!((selected.selected, selected.revision), (ConversationMode::Code, 2));
    assert!(matches!((selected.active, selected.pending),
        (ConversationMode::Chat, true) | (ConversationMode::Code, false)));
    let source = std::fs::read_to_string(&path).unwrap();
    let cold = session::JsonlWriter::session_mode_transition_from_source(&source, &selected.session_id).unwrap();
    assert_eq!((cold.active, cold.selected, cold.revision, cold.pending),
        (selected.active, selected.selected, selected.revision, selected.pending));
    // Compaction may have no messages, but its safe boundary must admit the
    // queued return to Code before deciding whether there is work to compact.
    let _ = orch.force_compact().await;
    let state = orch.conversation_mode().await;
    assert_eq!((state.active, state.selected, state.revision, state.pending),
        (ConversationMode::Code, ConversationMode::Code, 2, false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_compaction_during_pending_profile_reset_finishes_before_next_selection() {
    let root = tempfile::tempdir().unwrap();
    let (orch, _, path) = durable_orchestrator(root.path());
    let turn = orch.turn_gate.lock().await;
    assert!(orch.set_conversation_mode(ConversationMode::Chat).await.unwrap().pending);
    drop(turn);
    let snapshot_guard = orch.prompt_runtime.prompt_snapshot.lock().await;
    let owner = orch.clone();
    let compact = tokio::spawn(async move { owner.force_compact().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let source = std::fs::read_to_string(&path).unwrap_or_default();
            if source.lines().any(|line| line.contains("\"admittedMode\":\"chat\"") && line.contains("\"pending\":false")) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    compact.abort();
    let _ = compact.await;
    assert!(orch.turn_gate.try_lock().is_err());
    let next_owner = orch.clone();
    let selection = tokio::spawn(async move { next_owner.set_conversation_mode(ConversationMode::Code).await });
    tokio::task::yield_now().await;
    assert!(!selection.is_finished());
    drop(snapshot_guard);
    let selected = tokio::time::timeout(std::time::Duration::from_secs(5), selection).await.unwrap().unwrap().unwrap();
    assert_eq!((selected.selected, selected.revision), (ConversationMode::Code, 2));
    assert!(matches!((selected.active, selected.pending),
        (ConversationMode::Chat, true) | (ConversationMode::Code, false)));
    let source = std::fs::read_to_string(path).unwrap();
    let cold = session::JsonlWriter::session_mode_transition_from_source(&source, &selected.session_id).unwrap();
    assert_eq!((cold.active, cold.selected, cold.revision, cold.pending),
        (selected.active, selected.selected, selected.revision, selected.pending));
}

struct PausedFirstStream {
    inner: crate::test_support_stream::MockStreamingApiClient,
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl crate::conversation::StreamingApiClient for PausedFirstStream {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<lingxi_core::types::ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<futures::stream::BoxStream<'static, Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>>, llm_runtime::LlmError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        crate::conversation::StreamingApiClient::stream(&self.inner, model, profile, system, messages, tools, query_source,
            skip_global_cache_for_system_prompt, admission).await
    }
}

#[derive(Default)]
struct ObserveProfileTool {
    owner: std::sync::OnceLock<std::sync::Weak<ConversationOrchestrator>>,
    observed: tokio::sync::Mutex<Vec<(ConversationMode, u64)>>,
}

#[async_trait::async_trait]
impl tool_api::Tool for ObserveProfileTool {
    fn name(&self) -> &str { "ObserveProfile" }
    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<lingxi_core::types::utf16_json::Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| tool_api::tool_trait::generated_builtin_result(&serde_json::json!({"type":"object","properties":{}})))
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool { true }
    fn max_result_size_chars(&self) -> usize { 1024 }
    fn is_concurrency_safe(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool { true }
    fn is_read_only(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool { true }
    async fn check_permissions(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection, _: &tool_api::ToolUseContext) -> permission::PermissionResult {
        permission::PermissionResult::Allow { reason: permission::PermissionDecisionReason::Other { reason: "test".into() },
            updated_input: None, update_destination: None, metadata: Default::default() }
    }
    async fn description(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection, _: &tool_api::tool_trait::DescriptionOptions) -> String { "observe profile".into() }
    async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> lingxi_core::types::utf16_json::Utf16JsonProjection { "".into() }
    async fn call(&self, _: lingxi_core::types::utf16_json::Utf16JsonProjection, _: tool_api::ToolUseContext, _: tool_api::ToolProgressSender) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        let owner = self.owner.get().unwrap().upgrade().unwrap();
        self.observed.lock().await.push((owner.conversation_profile().await.active, owner.profile_epoch().await));
        Ok(tool_api::ToolCallResult::from_data("observed".into()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_selection_never_interrupts_the_turn_and_next_query_admits_new_mode() {
    use crate::test_support_stream::{content_block_start_text, content_block_start_tool_use,
        content_block_stop, input_json_delta, message_delta_stop, message_start, message_stop, text_delta};
    let model = crate::config::DEFAULT_MODEL;
    let finish = |id| vec![message_start(id, model), content_block_start_text(0), text_delta(0, "done"),
        content_block_stop(0), message_delta_stop("end_turn"), message_stop()];
    let stream = Arc::new(PausedFirstStream {
        inner: crate::test_support_stream::MockStreamingApiClient::with_turns(vec![
            vec![message_start("first", model), content_block_start_tool_use(0, lingxi_core::types::ToolUseId::new(), "ObserveProfile"),
                input_json_delta(0, "{}"), content_block_stop(0), message_delta_stop("tool_use"), message_stop()],
            finish("follow-up"), finish("second-turn"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0), entered: Default::default(), release: Default::default(),
    });
    let tool = Arc::new(ObserveProfileTool::default());
    let mut registry = tool_api::ToolRegistry::new();
    registry.register_builtin(tool.clone());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        crate::OrchestratorConfig::default(), Arc::new(MockApiClient::new(Vec::new())), stream.clone(),
        Arc::new(registry), noop_hook_executor(), Arc::new(NoOpPermissionGate), Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()), std::env::temp_dir(),
    ));
    assert!(tool.owner.set(Arc::downgrade(&orch)).is_ok());
    let cancel = CancellationToken::new();
    let first_owner = orch.clone();
    let first_cancel = cancel.clone();
    let first = tokio::spawn(async move { first_owner.run_turn_streaming_with_cancel("first", first_cancel).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.entered.notified()).await.unwrap();
    let selected = tokio::time::timeout(std::time::Duration::from_secs(5), orch.set_conversation_mode(ConversationMode::Chat)).await.unwrap().unwrap();
    assert_eq!((selected.active, selected.selected, selected.pending, orch.profile_epoch().await),
        (ConversationMode::Code, ConversationMode::Chat, true, 0));
    assert!(!cancel.is_cancelled());
    stream.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), first).await.unwrap().unwrap().unwrap();
    assert!(matches!(outcome, TurnOutcome::EndTurn));
    assert_eq!(*tool.observed.lock().await, vec![(ConversationMode::Code, 0)]);
    assert_eq!((orch.conversation_mode().await.active, orch.profile_epoch().await), (ConversationMode::Code, 0));
    let calls = stream.inner.captured_calls().await;
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert!(call.system.as_ref().unwrap().display_text().contains("command-line coding assistant"));
    }
    orch.run_turn_streaming_with_cancel("second", CancellationToken::new()).await.unwrap();
    assert_eq!((orch.conversation_mode().await.active, orch.profile_epoch().await), (ConversationMode::Chat, 1));
    let calls = stream.inner.captured_calls().await;
    assert_eq!(calls.len(), 3);
    assert!(calls[2].system.as_ref().unwrap().display_text().contains("research, writing, learning"));
}
