use super::*;
use crate::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::{OrchestratorConfig, TurnOutcome};
use async_trait::async_trait;
use lingxi_core::host::task_notification::TaskNotificationProvider;
use lingxi_core::host::task_registry::TaskNotification;
use lingxi_core::host::{CancellationToken, FileSystem, NewDiagnosticsSource, OrchestratorHandle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const MODEL: &str = "claude-opus-4-7";

#[derive(Clone, Copy, Debug)]
enum Kind {
    Memory,
    Hook,
    GuardedHook,
}
impl Kind {
    fn name(self) -> &'static str {
        if matches!(self, Self::Memory) {
            "memory_update"
        } else {
            "async_hook_response"
        }
    }
    fn marker(self, suffix: &str) -> String {
        format!("CONSUMED_{}_{suffix}", self.name())
    }
    fn matches(self, message: &ConversationMessage, suffix: &str) -> bool {
        let text = message.text_content();
        text.contains(&self.marker(suffix))
            && (!matches!(self, Self::Memory)
                || text.contains("Background memory consolidation updated your memory directory"))
    }
}

struct Guard(CancellationToken);
impl HookPublicationGuard for Guard {
    fn is_current(&self) -> bool {
        !self.0.is_cancelled()
    }
    fn generation_cancellation_token(&self) -> Option<CancellationToken> {
        Some(self.0.clone())
    }
    fn publish_if_current<'a>(
        &'a self,
        work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if !self.is_current() {
                return false;
            }
            work.await;
            self.is_current()
        })
    }
    fn commit_if_current<'a>(
        &'a self,
        work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        self.publish_if_current(work)
    }
}

#[derive(Default)]
struct Sources {
    tasks: Mutex<Vec<TaskNotification>>,
    hooks: Mutex<Vec<crate::prompt::async_hook_response::AsyncHookResponse>>,
    cancel_on_drain: Mutex<Option<CancellationToken>>,
}
impl Sources {
    fn push(&self, kind: Kind, suffix: &str, generation: &CancellationToken) {
        if matches!(kind, Kind::Memory) {
            self.tasks.lock().unwrap().push(TaskNotification {
                task_id: format!("dream-{suffix}"),
                task_type: "dream".into(),
                status: "completed".into(),
                description: "memory consolidation".into(),
                result: Some(kind.marker(suffix).into()),
                ..Default::default()
            });
        } else {
            self.hooks.lock().unwrap().push(
                crate::prompt::async_hook_response::AsyncHookResponse {
                    text: kind.marker(suffix).into(),
                    hook_event: Some("PostToolUse".into()),
                    publication_guard: matches!(kind, Kind::GuardedHook).then(|| {
                        Arc::new(Guard(generation.clone())) as Arc<dyn HookPublicationGuard>
                    }),
                },
            );
        }
    }
}
#[async_trait]
impl TaskNotificationProvider for Sources {
    async fn take_pending_task_notifications(&self) -> Vec<TaskNotification> {
        let rows = std::mem::take(&mut *self.tasks.lock().unwrap());
        if !rows.is_empty() {
            if let Some(token) = self.cancel_on_drain.lock().unwrap().take() {
                token.cancel();
            }
        }
        rows
    }
}
#[async_trait]
impl crate::prompt::async_hook_response::AsyncHookResponseProvider for Sources {
    async fn take_pending_responses(&self) -> Vec<hooks::ExactHookText> {
        self.take_pending_with_events()
            .await
            .into_iter()
            .map(|row| row.text)
            .collect()
    }
    async fn take_pending_with_events(
        &self,
    ) -> Vec<crate::prompt::async_hook_response::AsyncHookResponse> {
        let rows = std::mem::take(&mut *self.hooks.lock().unwrap());
        if !rows.is_empty() {
            if let Some(token) = self.cancel_on_drain.lock().unwrap().take() {
                token.cancel();
            }
        }
        rows
    }
}

#[derive(Default)]
struct CancelAfterCollection(Mutex<Option<CancellationToken>>);
#[async_trait]
impl NewDiagnosticsSource for CancelAfterCollection {
    async fn take_new_diagnostics_block(&self) -> Option<Source> {
        if let Some(token) = self.0.lock().unwrap().take() {
            token.cancel();
        }
        None
    }
}

struct PreparedHotTarget(Arc<session::jsonl::durable_writer::DurableTranscriptWriter>);
#[async_trait]
impl crate::conversation::CostSessionSwitcher for PreparedHotTarget {
    async fn prepare_session(
        &self,
        tracker: Arc<cost::CostTracker>,
        session: SessionId,
    ) -> Result<crate::conversation::PreparedSessionSwitch, cost::CostPersistError> {
        Ok(crate::conversation::PreparedSessionSwitch::new(
            tracker.prepare_session(session).await?,
            Some(self.0.clone()),
        ))
    }
}
struct Selector;
#[async_trait]
impl sidequery::SideQueryClient for Selector {
    async fn query(
        &self,
        _: sidequery::SideQueryRequest,
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
struct Runtime;
#[async_trait]
impl lingxi_core::host::RuntimeSpawner for Runtime {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        tokio::spawn(task);
        Ok(lingxi_core::host::BackgroundTaskHandle {
            task_name: name.into(),
            task_id: 0,
        })
    }
    async fn sleep(&self, _: std::time::Duration) {}
    async fn cancel(
        &self,
        _: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

struct Fixture {
    _cost_records: tokio::sync::mpsc::Receiver<cost::CostState>,
    _root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    transcript: PathBuf,
    fs: Arc<dyn FileSystem>,
    config: OrchestratorConfig,
    session: SessionId,
    sources: Arc<Sources>,
    cancel: Arc<CancelAfterCollection>,
    generation: CancellationToken,
    prefetch: Arc<memory::prefetch::MemoryPrefetch>,
    api: Arc<MockApiClient>,
    stream: Arc<MockStreamingApiClient>,
    registry: Arc<tokio::sync::RwLock<hooks::HookRegistry>>,
    tools: Arc<tool_api::registry::ToolRegistry>,
    read_state: tool_api::read_file_state::ReadFileStateMap,
    orch: Arc<ConversationOrchestrator>,
}

fn writer(
    fs: Arc<dyn FileSystem>,
    path: &std::path::Path,
    session: SessionId,
    cwd: &std::path::Path,
    home: &std::path::Path,
) -> Arc<session::JsonlWriter> {
    let authority = home.join("writer-authority");
    std::fs::create_dir_all(&authority).unwrap();
    let lock =
        Arc::new(session::jsonl::durable_writer::DurableTranscriptWriter::open(authority).unwrap());
    let writer = session::JsonlWriter::new(path.to_path_buf(), fs).with_durable_lock(lock.clone());
    // A rebuilt durable host admits the existing physical source before
    // loading it. An already-present header alone does not grant authority.
    let captured = path.exists().then(|| {
        session::jsonl::writer::TranscriptFileAuthority::capture(
            path.parent().unwrap().to_path_buf(),
            path.file_name().unwrap().into(),
        )
        .unwrap()
        .0
    });
    writer
        .activate_session_target_with_durable_lock_and_authority(
            session,
            path.to_path_buf(),
            cwd.to_path_buf(),
            lock,
            captured,
        )
        .unwrap();
    Arc::new(writer)
}

impl Fixture {
    async fn new(read_first: bool) -> Self {
        Self::new_with_steering(read_first, None).await
    }

    async fn new_with_steering(read_first: bool, steering: Option<Kind>) -> Self {
        Self::new_with_terminal(read_first, steering, false).await
    }

    async fn new_with_terminal(read_first: bool, steering: Option<Kind>, completed: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let cwd = canonical.join("repo");
        let home = canonical.join("home");
        let memdir = canonical.join("memory");
        for path in [&cwd, &home, &memdir] {
            std::fs::create_dir_all(path).unwrap();
        }
        let trigger = cwd.join("probe.txt");
        std::fs::write(&trigger, "real Read completes before steering").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(canonical));
        let session = SessionId::new();
        let (cost_records, cost_rx) = tokio::sync::mpsc::channel(128);
        let tracker = Arc::new(cost::CostTracker::new(
            session,
            Arc::new(cost::PricingCatalog::empty()),
            cost_records,
        ));
        let transcript = session::jsonl::path::session_path(
            &home,
            &cwd.to_string_lossy(),
            &session.as_uuid().to_string(),
        );
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let responses = (0..10)
            .map(|index| {
                let mut reply = if read_first && index == 0 {
                    mock_message_response(
                        vec![llm_runtime::ContentBlock::ToolCall {
                            id: "read-before-steering".into(),
                            name: "Read".into(),
                            input: lingxi_llm_client::protocol::JsonPayload::parse(
                                &serde_json::json!({"file_path":trigger}).to_string(),
                            )
                            .unwrap(),
                        }],
                        Some("tool_use"),
                    )
                } else {
                    mock_message_response(
                        vec![llm_runtime::ContentBlock::Text {
                            text: "done".into(),
                            cache_control: None,
                            citations: None,
                        }],
                        Some("end_turn"),
                    )
                };
                reply.id = format!("consumed-{index}");
                reply.model = MODEL.into();
                reply
            })
            .collect::<Vec<_>>();
        let streams = responses
            .iter()
            .enumerate()
            .map(|(index, reply)| {
                if completed {
                    return vec![llm_runtime::HistoryEvent::Completed {
                        response: Box::new(reply.clone()),
                    }];
                }
                let mut events = vec![message_start(&format!("consumed-{index}"), MODEL)];
                match &reply.content[0] {
                    llm_runtime::ContentBlock::ToolCall { id, name, input } => {
                        events.push(content_block_start_tool_use(0, id.clone().into(), name));
                        events.push(input_json_delta(0, input.as_json_source()));
                    }
                    _ => {
                        events.push(content_block_start_text(0));
                        events.push(text_delta(0, "done"));
                    }
                }
                events.extend([
                    content_block_stop(0),
                    message_delta_stop(reply.stop_reason.as_deref().unwrap()),
                    message_stop(),
                ]);
                events
            })
            .collect();
        let api = Arc::new(MockApiClient::new(responses));
        let stream = Arc::new(MockStreamingApiClient::with_turns(streams));
        let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
            Arc::new(memory::selector::MemorySelector::new(Arc::new(Selector))),
            Arc::new(Runtime),
            memory::memdir::MemdirRoots {
                user_memdir: memdir,
                session_memdir: home.join("session-memory"),
                team_memdir: None,
            },
        ));
        let sources = Arc::new(Sources::default());
        let cancel = Arc::new(CancelAfterCollection::default());
        let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
        let config = OrchestratorConfig {
            model: MODEL.into(),
            system_prompt_override: Some("consume-once recovery".into()),
            ..Default::default()
        };
        let read_state = tool_api::read_file_state::new_read_file_state_map();
        let mut context = tool_api::test_support::ctx_for_file_tools(
            fs.clone(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![cwd.clone()],
        );
        context.read_file_state = read_state.clone();
        let mut tools = tool_api::registry::ToolRegistry::new();
        tools.register_builtin(Arc::new(tool_file::FileReadTool::new(context)));
        let tools = Arc::new(tools);
        let initial_writer = writer(fs.clone(), &transcript, session, &cwd, &home);
        let prepared_target = Arc::new(PreparedHotTarget(Arc::new(
            session::jsonl::durable_writer::DurableTranscriptWriter::open(
                home.join("writer-authority"),
            )
            .unwrap(),
        )));
        let mut orch = ConversationOrchestrator::new_with_streaming(
            config.clone(),
            api.clone(),
            stream.clone(),
            tools.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_session_id(session)
        .with_config_home(home.clone())
        .with_jsonl_writer(initial_writer)
        .with_cost_tracker(tracker)
        .with_cost_session_switcher(prepared_target)
        .with_memory_prefetch(prefetch.clone())
        .with_task_notifications(sources.clone())
        .with_async_hook_responses(sources.clone())
        .with_new_diagnostics_source(cancel.clone())
        .with_hook_registry(registry.clone())
        .with_read_state_map(read_state.clone());
        let generation = CancellationToken::new();
        if let Some(kind) = steering {
            orch = orch.with_mid_turn_input(Arc::new(SteeringAfterRead {
                read_state: read_state.clone(),
                trigger: trigger.clone(),
                sources: sources.clone(),
                generation: generation.clone(),
                kind,
                delivered: AtomicBool::new(false),
            }));
        }
        let orch = ConversationOrchestrator::into_shared(orch);
        orch.initialize_conversation_mode().await.unwrap();
        Self {
            _cost_records: cost_rx,
            _root: root,
            cwd,
            home,
            transcript,
            fs,
            config,
            session,
            sources,
            cancel,
            generation,
            prefetch,
            api,
            stream,
            registry,
            tools,
            read_state,
            orch,
        }
    }

    async fn run(&self, streaming: bool, prompt: &str) {
        if streaming {
            self.orch.run_turn_streaming(prompt).await.unwrap();
        } else {
            self.orch.run_turn(prompt).await.unwrap();
        }
    }
    async fn interrupt(&self, streaming: bool) {
        let token = CancellationToken::new();
        *self.cancel.0.lock().unwrap() = Some(token.clone());
        let result = if streaming {
            self.orch
                .run_turn_streaming_with_cancel("cancel after collection", token)
                .await
                .unwrap()
        } else {
            self.orch
                .run_turn_with_cancel("cancel after collection", token)
                .await
                .unwrap()
        };
        assert!(matches!(result, TurnOutcome::Cancelled));
    }
    async fn calls(&self, streaming: bool) -> Vec<Vec<ConversationMessage>> {
        if streaming {
            self.stream
                .captured_calls()
                .await
                .into_iter()
                .map(|call| call.messages)
                .collect()
        } else {
            self.api.captured_msgs().await
        }
    }
    async fn recover(&mut self, mode: u8) {
        if mode == 0 {
            return;
        }
        let live = self.orch.capture_pending_transient_reminder_authority();
        self.recover_with_authority(mode, live).await;
    }
    async fn recover_with_authority(
        &mut self,
        mode: u8,
        live: Option<lingxi_core::host::ResumeTransientReminderAuthority>,
    ) {
        if mode == 1 {
            let replay = crate::resume::replay_session_state(
                &self.home,
                &self.cwd.to_string_lossy(),
                self.session.as_uuid(),
                self.fs.clone(),
            )
            .await
            .unwrap();
            let mut runtime = replay.handle_runtime_snapshot();
            runtime.pending_transient_reminder_authority = live;
            self.orch
                .resume_session(
                    self.session,
                    replay.state.history,
                    replay.last_message_uuid.map(|id| id.to_string()),
                    None,
                    runtime,
                )
                .await
                .unwrap();
        } else {
            let mut cold = ConversationOrchestrator::with_resume(
                self.config.clone(),
                self.session.as_uuid(),
                self.home.clone(),
                self.cwd.to_string_lossy().into_owned(),
                self.fs.clone(),
                self.api.clone(),
                self.tools.clone(),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                self.cwd.clone(),
                Some(writer(
                    self.fs.clone(),
                    &self.transcript,
                    self.session,
                    &self.cwd,
                    &self.home,
                )),
            )
            .await
            .unwrap()
            .with_memory_prefetch(self.prefetch.clone())
            .with_task_notifications(self.sources.clone())
            .with_async_hook_responses(self.sources.clone())
            .with_new_diagnostics_source(self.cancel.clone())
            .with_hook_registry(self.registry.clone())
            .with_read_state_map(self.read_state.clone());
            cold.streaming_api = self.stream.clone();
            if mode == 2 {
                let snapshot = live
                    .as_ref()
                    .and_then(|bag| bag.get::<LiveConsumedReminderSnapshot>())
                    .expect("idle live owner exported its authority");
                assert_eq!(snapshot.source_path, Some(self.transcript.clone()));
                assert!(
                    cold.transcript
                        .jsonl_writer
                        .as_ref()
                        .unwrap()
                        .verify_active_transcript_source(
                            snapshot.source_authority.as_ref().unwrap()
                        )
                        .is_ok(),
                    "new writer must own the real source named by the live bag"
                );
                cold = cold.with_pending_transient_reminder_authority(live);
            }
            self.orch = ConversationOrchestrator::into_shared(cold);
        }
    }

    async fn add_mod(&self, code: &str) {
        let module = self.home.join("consume-mod.js");
        std::fs::write(&module, code).unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "consume-mod",
            &self.home,
            &module,
            tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
        )
        .await
        .unwrap();
        self.registry.write().await.set_mod_host(host);
    }
}

fn run_test(future: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future)
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn consumed_sources_survive_initial_cancel_and_live_hot_rebuild_or_cold_mount_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for kind in [Kind::Memory, Kind::Hook, Kind::GuardedHook] {
                for recovery in 0..4 {
                    let mut fixture = Fixture::new(false).await;
                    fixture.sources.push(kind, "A", &fixture.generation);
                    fixture.interrupt(streaming).await;
                    assert!(fixture.calls(streaming).await.is_empty());
                    let retained = fixture
                        .orch
                        .consumed_reminders_for_kind(fixture.session, kind.name());
                    let id = retained
                        .iter()
                        .find(|row| kind.matches(&row.message, "A"))
                        .unwrap()
                        .message
                        .id();
                    let original_owner = Arc::downgrade(&fixture.orch);
                    fixture.recover(recovery).await;
                    if recovery >= 2 {
                        assert!(original_owner.upgrade().is_none(), "recovery must not retain the old orchestrator through its authority bag");
                    }
                    fixture.sources.push(kind, "B", &fixture.generation);
                    fixture
                        .run(streaming, "continue with a newly completed event too")
                        .await;
                    fixture.run(streaming, "unchanged sources stay quiet").await;
                    let calls = fixture.calls(streaming).await;
                    let first = &calls[0];
                    let delivered = first
                        .iter()
                        .filter(|row| kind.matches(row, "A"))
                        .collect::<Vec<_>>();
                    if recovery == 3 && matches!(kind, Kind::GuardedHook) {
                        assert!(
                            delivered.is_empty(),
                            "cold metadata cannot manufacture the old live executor authority"
                        );
                    } else {
                        assert_eq!(
                            delivered.len(),
                            1,
                            "{streaming} {kind:?} recovery={recovery}"
                        );
                        assert_eq!(delivered[0].id(), id, "retry preserves identity");
                    }
                    assert_eq!(first.iter().filter(|row| kind.matches(row, "B")).count(), 1);
                    assert!(!calls[1]
                        .iter()
                        .any(|row| kind.matches(row, "A") || kind.matches(row, "B")));
                    assert!(
                        !fixture
                            .orch
                            .snapshot_history()
                            .await
                            .iter()
                            .any(|row| kind.matches(row, "A") || kind.matches(row, "B")),
                        "consume-once rows remain transient"
                    );
                    // Reopen once more after ACK: an empty checkpoint must prevent resurrection.
                    fixture.recover(3).await;
                    fixture
                        .run(streaming, "cold after successful delivery")
                        .await;
                    assert!(!fixture
                        .calls(streaming)
                        .await
                        .last()
                        .unwrap()
                        .iter()
                        .any(|row| kind.matches(row, "A") || kind.matches(row, "B")));
                }
            }
        }
    });
}

#[test]
fn later_cancel_keeps_only_unsent_rows_and_family_order_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            let fixture = Fixture::new(false).await;
            for kind in [Kind::Memory, Kind::Hook] {
                fixture.sources.push(kind, "OLD", &fixture.generation);
            }
            fixture.run(streaming, "first normal delivery").await;
            for kind in [Kind::Memory, Kind::Hook] {
                fixture.sources.push(kind, "A", &fixture.generation);
            }
            fixture.interrupt(streaming).await;
            assert_eq!(fixture.calls(streaming).await.len(), 1);
            for kind in [Kind::Memory, Kind::Hook] {
                fixture.sources.push(kind, "B", &fixture.generation);
            }
            fixture.run(streaming, "continue later cancellation").await;
            let calls = fixture.calls(streaming).await;
            let rows = &calls[1];
            let ordered = rows
                .iter()
                .filter_map(|row| {
                    [Kind::Hook, Kind::Memory].into_iter().find_map(|kind| {
                        ["A", "B"]
                            .into_iter()
                            .find(|suffix| kind.matches(row, suffix))
                            .map(|suffix| format!("{}-{suffix}", kind.name()))
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(
                ordered,
                [
                    "async_hook_response-A",
                    "async_hook_response-B",
                    "memory_update-A",
                    "memory_update-B"
                ]
            );
            assert!(!rows
                .iter()
                .any(|row| Kind::Hook.matches(row, "OLD") || Kind::Memory.matches(row, "OLD")));
        }
    });
}

#[test]
fn rewritten_sources_keep_raw_identity_and_explicit_suppression_does_not_resurrect_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for kind in [Kind::Memory, Kind::Hook] {
                for suppress in [false, true] {
                    let mut fixture = Fixture::new(false).await;
                    fixture.add_mod(&format!("export function register(on) {{on('prompt.attachment', {{type:'{}'}}, {});}}",kind.name(),
                if suppress {"()=>({text:null})"} else {"($,e,next)=>next({...e,text:'MOD:'+e.text})"})).await;
                    fixture.sources.push(kind, "A", &fixture.generation);
                    fixture.interrupt(streaming).await;
                    fixture.recover(1).await;
                    fixture
                        .run(streaming, "continue after attachment customization")
                        .await;
                    fixture.run(streaming, "no duplicate customization").await;
                    let calls = fixture.calls(streaming).await;
                    let selected = calls[0]
                        .iter()
                        .filter(|row| kind.matches(row, "A"))
                        .collect::<Vec<_>>();
                    assert_eq!(selected.len(), usize::from(!suppress));
                    if !suppress {
                        assert_eq!(
                            selected[0].text_content().matches("MOD:").count(),
                            1,
                            "raw source must not be transformed twice"
                        );
                    }
                    assert!(!calls[1].iter().any(|row| kind.matches(row, "A")));
                    fixture.registry.write().await.clear_mod_host();
                    fixture.recover(3).await;
                    fixture.run(streaming, "cold with the Mod gone").await;
                    assert!(!fixture
                        .calls(streaming)
                        .await
                        .last()
                        .unwrap()
                        .iter()
                        .any(|row| kind.matches(row, "A")));
                }
            }
        }
    });
}

#[test]
fn invalidated_hook_guard_is_filtered_after_cancel_and_hot_restore_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for recovery in [0, 1, 2, 3] {
                let mut fixture = Fixture::new(false).await;
                fixture
                    .sources
                    .push(Kind::GuardedHook, "RETIRED", &fixture.generation);
                fixture.interrupt(streaming).await;
                fixture.generation.cancel();
                fixture.recover(recovery).await;
                fixture
                    .run(streaming, "continue after source retirement")
                    .await;
                assert!(!fixture
                    .calls(streaming)
                    .await
                    .iter()
                    .flatten()
                    .any(|row| Kind::Hook.matches(row, "RETIRED")));
            }
        }
    });
}

#[test]
fn synthetic_mod_answers_do_not_ack_unsent_consume_once_sources_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for kind in [Kind::Memory, Kind::Hook] {
                let fixture = Fixture::new(false).await;
                fixture.add_mod(r#"export function register(on) {let first=true;
                on('turn.step',async function*($,e,next){if(first){first=false;
                    yield {kind:'text',index:0,text:'synthetic'};yield {kind:'stop',stopReason:'end_turn',usage:null};
                    return {turnId:e.turnId,index:e.index,answer:'synthetic',toolUses:[],stopReason:'end_turn',usage:null};}
                    const below=next(e);for await(const chunk of below)yield chunk;return await below.result;});}"#).await;
                fixture.sources.push(kind, "A", &fixture.generation);
                fixture.run(streaming, "synthetic first").await;
                assert!(fixture.calls(streaming).await.is_empty());
                fixture.run(streaming, "physical next").await;
                fixture.run(streaming, "quiet next").await;
                let calls = fixture.calls(streaming).await;
                assert_eq!(
                    calls[0].iter().filter(|row| kind.matches(row, "A")).count(),
                    1
                );
                assert!(!calls[1].iter().any(|row| kind.matches(row, "A")));
            }
        }
    });
}

const PASS_THROUGH_STEP: &str = r#"export function register(on) {
    on('turn.step', async function*($, e, next) {
        const below = next(e); for await (const chunk of below) yield chunk;
        return await below.result;
    });
}"#;

#[test]
fn physical_mod_terminals_ack_retained_sources_after_cancel_and_hot_or_cold_recovery() {
    run_test(async {
        for streaming in [false, true] {
            for completed in [false, true] {
                if !streaming && completed {
                    continue;
                }
                for recovery in 0..4 {
                    let mut fixture = Fixture::new_with_terminal(false, None, completed).await;
                    fixture.add_mod(PASS_THROUGH_STEP).await;
                    for kind in [Kind::Hook, Kind::Memory] {
                        fixture.sources.push(kind, "A", &fixture.generation);
                    }
                    fixture.interrupt(streaming).await;
                    assert!(fixture.calls(streaming).await.is_empty());
                    let ids = [Kind::Hook, Kind::Memory].map(|kind| {
                        fixture
                            .orch
                            .consumed_reminders_for_kind(fixture.session, kind.name())
                            .into_iter()
                            .find(|row| kind.matches(&row.message, "A"))
                            .unwrap()
                            .message
                            .id()
                    });
                    fixture.recover(recovery).await;
                    for kind in [Kind::Hook, Kind::Memory] {
                        fixture.sources.push(kind, "B", &fixture.generation);
                    }
                    fixture.run(streaming, "continue after cancellation").await;
                    fixture.run(streaming, "same sources stay quiet").await;
                    let calls = fixture.calls(streaming).await;
                    assert_eq!(calls.len(), 2);
                    let mut order = Vec::new();
                    for row in &calls[0] {
                        for (index, kind) in [Kind::Hook, Kind::Memory].into_iter().enumerate() {
                            for suffix in ["A", "B"] {
                                if kind.matches(row, suffix) {
                                    if suffix == "A" {
                                        assert_eq!(row.id(), ids[index]);
                                    }
                                    order.push(format!("{}-{suffix}", kind.name()));
                                }
                            }
                        }
                    }
                    assert_eq!(
                        order,
                        [
                            "async_hook_response-A",
                            "async_hook_response-B",
                            "memory_update-A",
                            "memory_update-B"
                        ]
                    );
                    assert!(!calls[1].iter().any(|row| [Kind::Hook, Kind::Memory]
                        .into_iter()
                        .any(|kind| kind.matches(row, "A") || kind.matches(row, "B"))));
                    fixture.recover(3).await;
                    fixture
                        .run(streaming, "cold after successful delivery")
                        .await;
                    assert!(!fixture
                        .calls(streaming)
                        .await
                        .last()
                        .unwrap()
                        .iter()
                        .any(|row| [Kind::Hook, Kind::Memory]
                            .into_iter()
                            .any(|kind| kind.matches(row, "A") || kind.matches(row, "B"))));
                }
            }
        }
    });
}

#[test]
fn physical_mod_terminals_ack_sources_with_running_user_append_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for completed in [false, true] {
                if !streaming && completed {
                    continue;
                }
                for kind in [Kind::Memory, Kind::Hook] {
                    let fixture = Fixture::new_with_terminal(true, Some(kind), completed).await;
                    fixture.add_mod(PASS_THROUGH_STEP).await;
                    fixture.sources.push(kind, "BEFORE", &fixture.generation);
                    fixture
                        .run(streaming, "read before the steering input")
                        .await;
                    fixture.run(streaming, "no repeated reminders").await;
                    let calls = fixture.calls(streaming).await;
                    assert_eq!(calls.len(), 3);
                    assert_eq!(
                        calls[0]
                            .iter()
                            .filter(|row| kind.matches(row, "BEFORE"))
                            .count(),
                        1
                    );
                    assert!(!calls[0].iter().any(|row| kind.matches(row, "DURING_TOOLS")));
                    assert_eq!(
                        calls[1]
                            .iter()
                            .filter(|row| kind.matches(row, "DURING_TOOLS"))
                            .count(),
                        1
                    );
                    assert!(calls[1]
                        .iter()
                        .any(|row| row.text_content().contains("USER_STEERING_AFTER_REAL_READ")));
                    assert!(!calls[1].iter().any(|row| kind.matches(row, "BEFORE")));
                    assert!(!calls[2].iter().any(
                        |row| kind.matches(row, "BEFORE") || kind.matches(row, "DURING_TOOLS")
                    ));
                }
            }
        }
    });
}

struct SteeringAfterRead {
    read_state: tool_api::read_file_state::ReadFileStateMap,
    trigger: PathBuf,
    sources: Arc<Sources>,
    generation: CancellationToken,
    kind: Kind,
    delivered: AtomicBool,
}
#[async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for SteeringAfterRead {
    fn admits_at(&self, point: crate::prompt::mid_turn_input::MidTurnInputPoint) -> bool {
        // Both production drivers admit ordinary host user queues here. The
        // read-state predicate below ensures this appears during the turn.
        point == crate::prompt::mid_turn_input::MidTurnInputPoint::LoopStart
    }
    async fn take_mid_turn_input(&self) -> Option<Source> {
        if !self.read_state.lock().unwrap().contains(&self.trigger)
            || self.delivered.swap(true, Ordering::SeqCst)
        {
            return None;
        }
        self.sources
            .push(self.kind, "DURING_TOOLS", &self.generation);
        Some("USER_STEERING_AFTER_REAL_READ".into())
    }
}

#[test]
fn new_completion_with_running_user_append_reaches_the_next_step_once_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for kind in [Kind::Memory, Kind::Hook] {
                let fixture = Fixture::new_with_steering(true, Some(kind)).await;
                fixture.run(streaming, "read the file first").await;
                let calls = fixture.calls(streaming).await;
                assert_eq!(calls.len(), 2);
                assert!(!calls[0].iter().any(|row| kind.matches(row, "DURING_TOOLS")));
                assert_eq!(
                    calls[1]
                        .iter()
                        .filter(|row| kind.matches(row, "DURING_TOOLS"))
                        .count(),
                    1
                );
                assert!(calls[1]
                    .iter()
                    .any(|row| row.text_content().contains("USER_STEERING_AFTER_REAL_READ")));
                fixture.run(streaming, "a later turn stays quiet").await;
                assert!(!fixture
                    .calls(streaming)
                    .await
                    .last()
                    .unwrap()
                    .iter()
                    .any(|row| kind.matches(row, "DURING_TOOLS")));
            }
        }
    });
}

#[test]
fn an_old_live_bag_cannot_reintroduce_an_acknowledged_message_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for kind in [Kind::Memory, Kind::GuardedHook] {
                let mut fixture = Fixture::new(false).await;
                fixture.sources.push(kind, "A", &fixture.generation);
                fixture.interrupt(streaming).await;
                let stale = fixture
                    .orch
                    .capture_pending_transient_reminder_authority()
                    .unwrap();
                fixture
                    .run(streaming, "physically deliver the pending row")
                    .await;
                fixture
                    .orch
                    .import_consumed_reminder_authority(fixture.session, &stale);
                assert!(
                    fixture
                        .orch
                        .consumed_reminders_for_kind(fixture.session, kind.name())
                        .is_empty(),
                    "live ACK floor fences even a not-yet-persisted receipt"
                );
                fixture.recover_with_authority(2, Some(stale)).await;
                fixture
                    .run(streaming, "a rebuilt owner rejects the old bag too")
                    .await;
                let calls = fixture.calls(streaming).await;
                assert_eq!(
                    calls[0].iter().filter(|row| kind.matches(row, "A")).count(),
                    1
                );
                assert!(!calls[1].iter().any(|row| kind.matches(row, "A")));
            }
        }
    });
}

#[test]
fn live_authority_requires_an_idle_owner_and_the_same_physical_transcript_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            let fixture = Fixture::new(false).await;
            fixture
                .sources
                .push(Kind::GuardedHook, "PRIVATE", &fixture.generation);
            fixture.interrupt(streaming).await;
            let busy = fixture.orch.turn_gate.lock().await;
            assert!(fixture
                .orch
                .capture_pending_transient_reminder_authority()
                .is_none());
            drop(busy);
            let live = fixture.orch.capture_pending_transient_reminder_authority();
            let sibling_home = fixture.home.join("sibling-home");
            std::fs::create_dir_all(&sibling_home).unwrap();
            let sibling_path = session::jsonl::path::session_path(
                &sibling_home,
                &fixture.cwd.to_string_lossy(),
                &fixture.session.as_uuid().to_string(),
            );
            std::fs::create_dir_all(sibling_path.parent().unwrap()).unwrap();
            std::fs::copy(&fixture.transcript, &sibling_path).unwrap();
            let mut sibling = ConversationOrchestrator::with_resume(
                fixture.config.clone(),
                fixture.session.as_uuid(),
                sibling_home.clone(),
                fixture.cwd.to_string_lossy().into_owned(),
                fixture.fs.clone(),
                fixture.api.clone(),
                fixture.tools.clone(),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                fixture.cwd.clone(),
                Some(writer(
                    fixture.fs.clone(),
                    &sibling_path,
                    fixture.session,
                    &fixture.cwd,
                    &sibling_home,
                )),
            )
            .await
            .unwrap()
            .with_pending_transient_reminder_authority(live);
            sibling.streaming_api = fixture.stream.clone();
            let sibling = ConversationOrchestrator::into_shared(sibling);
            if streaming {
                sibling
                    .run_turn_streaming("a same UUID sibling is a different source")
                    .await
                    .unwrap();
            } else {
                sibling
                    .run_turn("a same UUID sibling is a different source")
                    .await
                    .unwrap();
            }
            assert!(!fixture
                .calls(streaming)
                .await
                .iter()
                .flatten()
                .any(|row| Kind::Hook.matches(row, "PRIVATE")));
        }
    });
}

#[test]
fn cancellation_at_the_destructive_source_boundary_finishes_recovery_publication_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            for kind in [Kind::Memory, Kind::Hook, Kind::GuardedHook] {
                for recovery in 0..if matches!(kind, Kind::GuardedHook) {
                    3
                } else {
                    4
                } {
                    let mut fixture = Fixture::new(false).await;
                    fixture.sources.push(kind, "AT_DRAIN", &fixture.generation);
                    let token = CancellationToken::new();
                    *fixture.sources.cancel_on_drain.lock().unwrap() = Some(token.clone());
                    let result = if streaming {
                        fixture
                            .orch
                            .run_turn_streaming_with_cancel("cancel inside source", token)
                            .await
                            .unwrap()
                    } else {
                        fixture
                            .orch
                            .run_turn_with_cancel("cancel inside source", token)
                            .await
                            .unwrap()
                    };
                    assert!(matches!(result, TurnOutcome::Cancelled));
                    assert!(fixture.calls(streaming).await.is_empty());
                    fixture.recover(recovery).await;
                    fixture
                        .run(streaming, "continue source-boundary cancellation")
                        .await;
                    assert_eq!(
                        fixture.calls(streaming).await[0]
                            .iter()
                            .filter(|row| kind.matches(row, "AT_DRAIN"))
                            .count(),
                        1,
                        "streaming={streaming} kind={kind:?} recovery={recovery}"
                    );
                    fixture
                        .run(streaming, "the recovered source stays quiet")
                        .await;
                    assert!(!fixture
                        .calls(streaming)
                        .await
                        .last()
                        .unwrap()
                        .iter()
                        .any(|row| kind.matches(row, "AT_DRAIN")));
                }
            }
        }
    });
}

#[test]
fn unrendered_memory_queue_restores_exact_sources_and_scan_floor_before_rendering_both_drivers() {
    run_test(async {
        for streaming in [false, true] {
            let mut fixture = Fixture::new(false).await;
            // A recoverable session has a real conversation anchor as well as
            // host metadata; pure metadata is intentionally not a session.
            fixture
                .run(streaming, "create a real cold-resume anchor")
                .await;
            let updates = vec![PendingMemoryUpdate {
                source: MemoryUpdateSource::Dream,
                summary: Source::parse(r#""UNRENDERED_EXACT_\ud800""#).unwrap(),
            }];
            *fixture
                .orch
                .prompt_runtime
                .pending_memory_updates
                .lock()
                .unwrap() = updates.clone();
            fixture
                .orch
                .prompt_runtime
                .last_memory_scan_ms
                .store(7, Ordering::Relaxed);
            fixture
                .orch
                .retain_unrendered_memory_updates(fixture.session);
            fixture
                .orch
                .persist_consumed_reminders(fixture.session)
                .await;
            fixture.recover(3).await;
            assert_eq!(
                *fixture
                    .orch
                    .prompt_runtime
                    .pending_memory_updates
                    .lock()
                    .unwrap(),
                updates
            );
            assert_eq!(
                fixture
                    .orch
                    .prompt_runtime
                    .last_memory_scan_ms
                    .load(Ordering::Relaxed),
                7
            );
            fixture
                .run(streaming, "render the recovered raw update")
                .await;
            assert_eq!(
                fixture
                    .calls(streaming)
                    .await
                    .iter()
                    .flatten()
                    .filter(|row| row.text_content().contains("UNRENDERED_EXACT_"))
                    .count(),
                1
            );
            assert!(fixture
                .orch
                .prompt_runtime
                .pending_memory_updates
                .lock()
                .unwrap()
                .is_empty());
            fixture.recover(3).await;
            fixture
                .run(streaming, "the acknowledged raw source cannot resurrect")
                .await;
            assert!(!fixture
                .calls(streaming)
                .await
                .last()
                .unwrap()
                .iter()
                .any(|row| row.text_content().contains("UNRENDERED_EXACT_")));
        }
    });
}
