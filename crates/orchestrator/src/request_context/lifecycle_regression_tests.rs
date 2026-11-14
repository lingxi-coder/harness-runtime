//! Real preparation/driver regressions for listing admission, compaction
//! cancellation, and catalogs published after the initial prompt snapshot.

use super::*;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, with_scripted_compactor, MockStreamingApiClient,
};
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct UnusedHttp;

#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused lifecycle fixture".into()))
    }

    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused lifecycle fixture".into()))
    }
}

struct UnusedRuntime;

#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _: &str,
        _: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused lifecycle fixture".into()))
    }

    async fn sleep(&self, _: Duration) {}

    async fn cancel(
        &self,
        _: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn hook_result(response: Option<HookResponse>) -> HookResult {
    HookResult {
        outcome: HookOutcome::Success,
        stdout: String::new().into(),
        stderr: String::new().into(),
        exit_code: None,
        response,
    }
}

async fn hooks_with(
    event: HookEventType,
    handler: Arc<dyn BuiltinHookHandler>,
) -> Arc<hooks::HookExecutorImpl> {
    let registry = Arc::new(tokio::sync::RwLock::new(HookRegistry::new()));
    registry.write().await.register(HookDefinition {
        id: HookId::new(),
        name: handler.id().into(),
        events: vec![event],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: handler.id().into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    });
    let mut executor =
        hooks::HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    executor.register_builtin(handler);
    Arc::new(executor)
}

struct BlockFirstPrompt(AtomicBool);

#[async_trait]
impl BuiltinHookHandler for BlockFirstPrompt {
    fn id(&self) -> &str {
        "block-first-listing-prompt"
    }

    async fn handle(&self, _: &HookEvent, _: &HookContext) -> HookResult {
        hook_result(self.0.swap(false, Ordering::SeqCst).then(|| HookResponse {
            decision: Some(HookDecision::Block),
            reason: Some("reject this prompt before request assembly".into()),
            ..Default::default()
        }))
    }
}

#[derive(Default)]
struct OneShotGate {
    claimed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl OneShotGate {
    async fn pause_once(&self) {
        if !self.claimed.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

#[async_trait]
impl lingxi_core::host::NewDiagnosticsSource for OneShotGate {
    async fn take_new_diagnostics_block(&self) -> Option<Utf16JsonProjection> {
        self.pause_once().await;
        None
    }
}

struct PostCompactGate(Arc<OneShotGate>);

#[async_trait]
impl BuiltinHookHandler for PostCompactGate {
    fn id(&self) -> &str {
        "pause-post-compact-commit"
    }

    async fn handle(&self, event: &HookEvent, _: &HookContext) -> HookResult {
        if matches!(event, HookEvent::PostCompact { .. }) {
            self.0.pause_once().await;
        }
        hook_result(None)
    }
}

fn replies(count: usize) -> Vec<llm_runtime::HistoryResponse> {
    (0..count)
        .map(|_| {
            mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "accepted lifecycle fixture".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            )
        })
        .collect()
}

fn streams(count: usize) -> Vec<Vec<llm_runtime::HistoryEvent>> {
    (0..count)
        .map(|index| {
            vec![
                message_start(&format!("lifecycle-{index}"), "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "accepted lifecycle fixture"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        })
        .collect()
}

fn listing_orchestrator(
    cwd: &Path,
    transcript: &Path,
    fs: Arc<dyn FileSystem>,
    hooks: Arc<hooks::HookExecutorImpl>,
) -> (
    ConversationOrchestrator,
    Arc<MockApiClient>,
    Arc<MockStreamingApiClient>,
) {
    let api = Arc::new(MockApiClient::new(replies(2)));
    let streaming = Arc::new(MockStreamingApiClient::with_turns(streams(2)));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(SkillListingGateTool { name: "Skill" }));
    tools.register_builtin(Arc::new(SkillListingGateTool { name: "Agent" }));
    let orchestrator = ConversationOrchestrator::new_with_streaming(
        crate::OrchestratorConfig {
            model: "claude-opus-4-7".into(),
            ..Default::default()
        },
        api.clone(),
        streaming.clone(),
        Arc::new(tools),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    )
    .with_skill_listing(Arc::new(SkillListingFixture))
    .with_jsonl_writer(Arc::new(JsonlWriter::new(transcript.to_owned(), fs)));
    (orchestrator, api, streaming)
}

#[tokio::test]
async fn cancelled_preparation_settles_every_publication_commit_handoff() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().to_owned();
    let (orch, _, _) = listing_orchestrator(
        &cwd,
        &cwd.join("handoff.jsonl"),
        Arc::new(PosixFileSystem::new(cwd.clone())),
        noop_hook_executor(),
    );
    scope_if_missing(&orch, async {
        let old_publication = changed_files_publication_scope();
        mark_changed_files_publication_started();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let entered = tokio::sync::Notify::new();
        let release = tokio::sync::Notify::new();
        let future = crate::turn_loop::await_prepare_with_ingress_commit_boundary(async {
            drop(old_publication);
            let first_commit = preparation_commit_scope();
            entered.notify_one();
            release.notified().await;
            drop(first_commit);
            let next_publication = changed_files_publication_scope();
            mark_changed_files_publication_started();
            entered.notify_one();
            release.notified().await;
            drop(next_publication);
            let last_commit = preparation_commit_scope();
            entered.notify_one();
            release.notified().await;
            drop(last_commit);
            Ok(())
        }, Some(&cancel));
        tokio::pin!(future);
        for phase in 0..3 {
            tokio::select! {
                biased;
                result = &mut future => panic!("cancel escaped unfinished phase {phase}: {result:?}"),
                () = entered.notified() => {}
            }
            assert!(futures::poll!(&mut future).is_pending(), "phase={phase}");
            release.notify_one();
        }
        assert!(future.await.is_err());
        assert!(pending_preparation_commit().is_none());
        assert!(pending_changed_files_publication().is_none());
        Ok(())
    }).await.unwrap();
}

async fn check_explicit_context_cancel_keeps_turn_ownership(streaming: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("explicit-context-finalize.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let (orch, api, streaming_api) =
        listing_orchestrator(&cwd, &transcript, fs.clone(), noop_hook_executor());
    let orch = ConversationOrchestrator::into_shared(orch);
    bind_test_durable_transcript_target(&orch, &cwd, &cwd.join("state"), &transcript);
    let placement = Arc::new(PromptIngressAppendTestGate::default());
    let admission = Arc::new(PromptIngressAppendTestGate::default());
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = PROMPT_INGRESS_PLACEMENT_TEST_GATE.scope(
        placement.clone(),
        PROMPT_INGRESS_ADMISSION_TEST_GATE.scope(admission.clone(), async {
            if streaming {
                orch.run_turn_streaming_with_request_context(
                    "cancel explicit owner",
                    Vec::new(),
                    None,
                    cancel.clone(),
                )
                .await
            } else {
                orch.run_turn_with_request_context(
                    "cancel explicit owner",
                    Vec::new(),
                    None,
                    cancel.clone(),
                )
                .await
            }
        }),
    );
    tokio::pin!(first);
    tokio::select! {
        () = placement.wait_until_entered() => {}
        result = &mut first => panic!("placement not reached: {result:?}"),
    }
    cancel.cancel();
    placement.resume();
    tokio::select! {
        () = admission.wait_until_entered() => {}
        result = &mut first => panic!("cancelled accepted batch not finalized: {result:?}"),
    }
    let owns_gate = orch.turn_gate.try_lock().is_err();
    let second = run(&orch, streaming, "queued after cancellation", None);
    tokio::pin!(second);
    let second_waited = futures::poll!(&mut second).is_pending();
    admission.resume();
    assert_eq!(first.await.unwrap(), crate::TurnOutcome::Cancelled);
    assert!(
        owns_gate,
        "explicit-context finalization released turn_gate before its durable batch"
    );
    assert!(second_waited);
    assert_complete_live_cold_context(&orch, &cwd, &transcript, fs).await;
    assert_eq!(second.await.unwrap(), RunOutcome::End);
    let calls = captured(&api, &streaming_api, streaming).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(listing_rows(&orch, &calls[0]).len(), 2);
}

#[test]
fn batched_explicit_context_cancellation_finalizes_before_the_next_turn() {
    run_with_large_stack(|| check_explicit_context_cancel_keeps_turn_ownership(false));
}

#[test]
fn streaming_explicit_context_cancellation_finalizes_before_the_next_turn() {
    run_with_large_stack(|| check_explicit_context_cancel_keeps_turn_ownership(true));
}

async fn check_cancelled_prefetch_delivery(streaming: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("cancel-prefetch.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let (orch, api, streaming_api) =
        listing_orchestrator(&cwd, &transcript, fs.clone(), noop_hook_executor());
    let memory = |name: &str, text: &str| memory::surfacing::SurfacedMemory {
        path: cwd.join(name),
        content: text.into(),
        age_days: 0,
        mtime: std::time::SystemTime::UNIX_EPOCH,
    };
    let first_memory = memory("a.md", "RECOVERY_MEMORY_A");
    let skill = skill::DiscoveredSkill {
        name: "typed-fixture".into(),
        description: "RECOVERY_SKILL".into(),
        short_id: None,
    };
    let skills = Arc::new(skill::SkillDiscoveryPrefetch::with_fixed_result(
        Arc::new(UnusedRuntime),
        vec![skill.clone(), skill],
    ));
    let gate = Arc::new(OneShotGate::default());
    let orch = ConversationOrchestrator::into_shared(
        orch.with_memory_prefetch(Arc::new(
            memory::prefetch::MemoryPrefetch::with_fixed_result(
                Arc::new(UnusedRuntime),
                vec![
                    first_memory.clone(),
                    memory("b.md", "RECOVERY_MEMORY_B"),
                    first_memory,
                ],
            ),
        ))
        .with_skill_discovery_prefetch(skills.clone())
        .with_new_diagnostics_source(gate.clone()),
    );
    bind_test_durable_transcript_target(&orch, &cwd, &cwd.join("state"), &transcript);
    // The driver clears stale prefetch handles when a new prompt is admitted.
    // Seed the actual write-pivot history so its fresh discovery is eligible.
    let write_id = lingxi_core::types::ToolUseId::new();
    orch.append_external_history_message(ConversationMessage::user(
        MessageId::new(),
        "earlier edit".into(),
    ))
    .await;
    orch.append_external_history_message(ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::ToolUse {
            id: write_id.clone(),
            name: "Edit".into(),
            input: Utf16JsonProjection::parse("{}").unwrap(),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
        per_turn_effort: None,
        resumable: None,
    })
    .await;
    let mut result = ConversationMessage::user(MessageId::new(), "".into());
    if let ConversationMessage::User { content, .. } = &mut result {
        *content = vec![ContentBlock::ToolResult {
            tool_use_id: write_id,
            content: "earlier edit completed".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }];
    }
    orch.append_external_history_message(result).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = run(
        &orch,
        streaming,
        "cancel collected prefetch",
        Some(cancel.clone()),
    );
    tokio::pin!(first);
    tokio::select! {
        () = gate.entered.notified() => {}
        result = &mut first => panic!("prefetch collection barrier not reached: {result:?}"),
    }
    // This is a real mid-turn user append while the first preparation owns
    // its collected reminders. Cancellation must preserve both inputs.
    orch.inject_user_text("STEERING_DURING_CANCEL", false).await;
    cancel.cancel();
    gate.release.notify_one();
    assert_eq!(first.await.unwrap(), RunOutcome::Cancelled);
    assert!(captured(&api, &streaming_api, streaming).await.is_empty());
    assert!(orch
        .prompt_runtime
        .surfaced_memory_paths
        .lock()
        .await
        .is_empty());
    assert!(orch
        .prompt_runtime
        .surfaced_skill_names
        .lock()
        .await
        .is_empty());
    assert_eq!(
        orch.pending_surfaced_memories().len(),
        2,
        "batch duplicates must be removed without acknowledging delivery"
    );
    assert_eq!(orch.pending_discovered_skills().unwrap().len(), 1);
    assert_complete_live_cold_context(&orch, &cwd, &transcript, fs).await;
    assert_eq!(
        run(&orch, streaming, "continue collected prefetch", None)
            .await
            .unwrap(),
        RunOutcome::End
    );
    let calls = captured(&api, &streaming_api, streaming).await;
    assert_eq!(calls.len(), 1);
    for text in [
        "RECOVERY_MEMORY_A",
        "RECOVERY_MEMORY_B",
        "RECOVERY_SKILL",
        "STEERING_DURING_CANCEL",
    ] {
        assert_eq!(
            calls[0]
                .iter()
                .filter(|row| row.text_content().contains(text))
                .count(),
            1,
            "{text}"
        );
    }
    assert_eq!(
        orch.prompt_runtime.surfaced_memory_paths.lock().await.len(),
        2
    );
    assert_eq!(
        orch.prompt_runtime.surfaced_skill_names.lock().await.len(),
        1
    );
    assert_eq!(
        run(&orch, streaming, "normal next turn", None)
            .await
            .unwrap(),
        RunOutcome::End
    );
    let calls = captured(&api, &streaming_api, streaming).await;
    assert_eq!(calls.len(), 2);
    assert!(
        !calls[1].iter().any(
            |row| ["RECOVERY_MEMORY_A", "RECOVERY_MEMORY_B", "RECOVERY_SKILL"]
                .iter()
                .any(|text| row.text_content().contains(text))
        ),
        "successful delivery must deduplicate the next model step"
    );
    let durable = std::fs::read_to_string(&transcript).unwrap();
    assert!(
        !durable.contains("RECOVERY_MEMORY_A") && !durable.contains("RECOVERY_SKILL"),
        "surfacing stays transient while ordinary user steering is durable"
    );
}

#[test]
fn batched_cancelled_prefetch_is_retried_once_after_live_user_append() {
    run_with_large_stack(|| check_cancelled_prefetch_delivery(false));
}

#[test]
fn streaming_cancelled_prefetch_is_retried_once_after_live_user_append() {
    run_with_large_stack(|| check_cancelled_prefetch_delivery(true));
}

#[test]
fn cancelled_later_listings_keep_their_ids_until_the_actual_request_is_acknowledged() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let (orch, _, _) = listing_orchestrator(
            &cwd,
            &cwd.join("later-listings.jsonl"),
            Arc::new(PosixFileSystem::new(cwd.clone())),
            noop_hook_executor(),
        );
        let first = scope_if_missing(&orch, async {
            Ok(vec![
                orch.skill_listing_reminder_message()
                    .await
                    .unwrap()
                    .unwrap(),
                orch.agent_listing_reminder_message().await.unwrap(),
            ])
        })
        .await
        .unwrap();
        assert!(orch.prompt_runtime.sent_skill_names.lock().await.is_empty());
        assert!(orch.prompt_runtime.sent_agent_names.lock().await.is_empty());
        let retry = scope_if_missing(&orch, async {
            Ok(vec![
                orch.skill_listing_reminder_message()
                    .await
                    .unwrap()
                    .unwrap(),
                orch.agent_listing_reminder_message().await.unwrap(),
            ])
        })
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&retry).unwrap()
        );
        orch.acknowledge_step_reminders(&retry).await;
        assert!(!orch.prompt_runtime.sent_skill_names.lock().await.is_empty());
        assert!(!orch.prompt_runtime.sent_agent_names.lock().await.is_empty());
        scope_if_missing(&orch, async {
            assert!(orch
                .skill_listing_reminder_message()
                .await
                .unwrap()
                .is_none());
            assert!(orch.agent_listing_reminder_message().await.is_none());
            Ok(())
        })
        .await
        .unwrap();
    });
}

struct HotListing(AtomicBool);
#[async_trait]
impl SkillListingProvider for HotListing {
    async fn skill_listing_snapshot(
        &self,
    ) -> Result<SkillListingSnapshot, lingxi_core::settings::SettingsError> {
        let mut snapshot = SkillListingFixture.skill_listing_snapshot().await?;
        if self.0.load(Ordering::Acquire) {
            let mut added = snapshot.entries[0].clone();
            added.name = "late-skill".into();
            added.display_name = "late-skill".into();
            added.description = "LATE_SKILL_DESCRIPTION".into();
            snapshot.entries.push(added);
        }
        Ok(snapshot)
    }
}

async fn check_later_hot_listing_cancellation(path: ModelCallPath) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("hot-listing-cancel.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let (orch, _, _) = listing_orchestrator(&cwd, &transcript, fs.clone(), noop_hook_executor());
    let skills = Arc::new(HotListing(AtomicBool::new(false)));
    let agents = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let gate = Arc::new(OneShotGate::default());
    gate.claimed.store(true, Ordering::Release);
    let orch = orch
        .with_skill_listing(skills.clone())
        .with_agent_catalog(agents.clone())
        .with_new_diagnostics_source(gate.clone());
    bind_test_durable_transcript_target(&orch, &cwd, &cwd.join("state"), &transcript);
    let prompt = ConversationMessage::user(MessageId::new(), "initial catalog".into());
    orch.session.lock().await.history.push(prompt.clone());
    orch.persist_message_to_jsonl(&prompt).await;
    REQUEST_CONTEXT.scope(request_prompt_context(prompt.id()), async {
        persist_prompt_ingress_after_user(&orch, prompt.id(), PromptIngressKind::RegularUser).await.unwrap();
        orch.prepare_turn_step(path, None, true, true, None).await.unwrap();
        assert!(prompt_ingress_batch_finalized());
        skills.0.store(true, Ordering::Release);
        let mut agent = agent::builtin_agent_definitions().remove(0);
        agent.agent_type = "late-agent".into();
        agent.when_to_use = "LATE_AGENT_DESCRIPTION".into();
        agents.write().await.push(agent);
        orch.inject_user_text("human steering after catalog reload", false).await;
        gate.claimed.store(false, Ordering::Release);
        let cancel = tokio_util::sync::CancellationToken::new();
        let interrupted = crate::turn_loop::await_prepare_with_ingress_commit_boundary(
            orch.prepare_turn_step(path, None, false, true, Some(&cancel)), Some(&cancel));
        tokio::pin!(interrupted);
        tokio::select! {
            () = gate.entered.notified() => {}
            result = &mut interrupted => panic!("later preparation did not reach its delivery gate: {:?}", result.err()),
        }
        cancel.cancel();
        gate.release.notify_one();
        assert!(interrupted.await.is_err());
        assert!(!orch.prompt_runtime.sent_skill_names.lock().await.contains(&"late-skill".encode_utf16().collect::<Vec<_>>()));
        assert!(!orch.prompt_runtime.sent_agent_names.lock().await.contains("late-agent"));
        let retained = [orch.skill_listing_reminder_message().await.unwrap().unwrap(),
            orch.agent_listing_reminder_message().await.unwrap()];
        let continued = orch.prepare_turn_step(path, None, false, true, None).await.unwrap();
        for row in &retained {
            assert_eq!(continued.snapshot.iter().filter(|candidate| candidate.id() == row.id()).count(), 1);
        }
        assert!(continued.snapshot.iter().any(|row| row.text_content().contains("LATE_SKILL_DESCRIPTION")));
        assert!(continued.snapshot.iter().any(|row| row.text_content().contains("LATE_AGENT_DESCRIPTION")));
        orch.acknowledge_step_reminders(&continued.snapshot).await;
        assert!(orch.skill_listing_reminder_message().await.unwrap().is_none());
        assert!(orch.agent_listing_reminder_message().await.is_none());
        assert_complete_live_cold_context(&orch, &cwd, &transcript, fs).await;
    }).await;
}

#[test]
fn batched_hot_skill_and_agent_listing_survive_cancelled_later_step() {
    run_with_large_stack(|| check_later_hot_listing_cancellation(ModelCallPath::Batched));
}

#[test]
fn streaming_hot_skill_and_agent_listing_survive_cancelled_later_step() {
    run_with_large_stack(|| check_later_hot_listing_cancellation(ModelCallPath::Streaming));
}

#[derive(Debug, PartialEq)]
enum RunOutcome {
    End,
    Blocked,
    Cancelled,
}

async fn run(
    orchestrator: &ConversationOrchestrator,
    streaming: bool,
    prompt: &str,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<RunOutcome, crate::OrchestratorError> {
    if let Some(cancel) = cancel {
        let outcome = if streaming {
            orchestrator
                .run_turn_streaming_with_cancel(prompt, cancel)
                .await?
        } else {
            orchestrator.run_turn_with_cancel(prompt, cancel).await?
        };
        return Ok(match outcome {
            crate::TurnOutcome::EndTurn => RunOutcome::End,
            crate::TurnOutcome::Cancelled => RunOutcome::Cancelled,
            other => panic!("unexpected lifecycle outcome: {other:?}"),
        });
    }
    let outcome = if streaming {
        orchestrator.run_turn_streaming(prompt).await?
    } else {
        orchestrator.run_turn(prompt).await?
    };
    Ok(match outcome {
        crate::ConversationOutcome::EndTurn { .. } => RunOutcome::End,
        crate::ConversationOutcome::StopHookPrevented { .. } => RunOutcome::Blocked,
    })
}

async fn captured(
    api: &MockApiClient,
    streaming_api: &MockStreamingApiClient,
    streaming: bool,
) -> Vec<Vec<ConversationMessage>> {
    if streaming {
        streaming_api
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect()
    } else {
        api.captured_msgs().await
    }
}

fn listing_rows(
    orchestrator: &ConversationOrchestrator,
    messages: &[ConversationMessage],
) -> Vec<(MessageId, String)> {
    let attachments = orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap();
    messages
        .iter()
        .filter_map(|row| {
            let kind = attachments.get(&row.id())?.string_at("/type")?;
            matches!(kind, "skill_listing" | "agent_listing_delta")
                .then(|| (row.id(), kind.to_owned()))
        })
        .collect()
}

async fn check_listing_rejected_prompt(streaming: bool, cancel_preassembly: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("listing-admission.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let hook = if cancel_preassembly {
        noop_hook_executor()
    } else {
        hooks_with(
            HookEventType::UserPromptSubmit,
            Arc::new(BlockFirstPrompt(AtomicBool::new(true))),
        )
        .await
    };
    let (mut orchestrator, api, streaming_api) =
        listing_orchestrator(&cwd, &transcript, fs.clone(), hook);
    let gate = Arc::new(OneShotGate::default());
    if cancel_preassembly {
        orchestrator = orchestrator.with_new_diagnostics_source(gate.clone());
    }
    let orchestrator = ConversationOrchestrator::into_shared(orchestrator);
    bind_test_durable_transcript_target(&orchestrator, &cwd, &cwd.join("state"), &transcript);
    let session_id = orchestrator.session.lock().await.session_id;
    if cancel_preassembly {
        let cancel = tokio_util::sync::CancellationToken::new();
        let first = run(
            &orchestrator,
            streaming,
            "cancel this listing",
            Some(cancel.clone()),
        );
        tokio::pin!(first);
        tokio::select! {
            () = gate.entered.notified() => {}
            result = &mut first => panic!("preparation failed before its gate: {result:?}"),
        }
        cancel.cancel();
        gate.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("cancelled preparation settles")
            .unwrap();
        assert_eq!(result, RunOutcome::Cancelled);
    } else {
        let first = run(&orchestrator, streaming, "block this listing", None)
            .await
            .unwrap();
        assert_eq!(first, RunOutcome::Blocked);
        assert!(orchestrator
            .prompt_runtime
            .sent_skill_names
            .lock()
            .await
            .is_empty());
        assert!(orchestrator
            .prompt_runtime
            .sent_agent_names
            .lock()
            .await
            .is_empty());
    }
    assert!(captured(&api, &streaming_api, streaming).await.is_empty());
    run(&orchestrator, streaming, "first accepted prompt", None)
        .await
        .unwrap();
    run(&orchestrator, streaming, "next accepted prompt", None)
        .await
        .unwrap();
    assert_eq!(orchestrator.session.lock().await.session_id, session_id);
    let calls = captured(&api, &streaming_api, streaming).await;
    assert_eq!(calls.len(), 2);
    let first_rows = listing_rows(&orchestrator, &calls[0]);
    assert_eq!(
        first_rows
            .iter()
            .map(|(_, kind)| kind.as_str())
            .collect::<Vec<_>>(),
        ["agent_listing_delta", "skill_listing"],
        "the real listing producers must deliver exactly once in native order"
    );
    assert_eq!(listing_rows(&orchestrator, &calls[1]), first_rows);
    let durable = JsonlReader::new(transcript, fs).read_all().await.unwrap();
    for (id, _) in first_rows {
        assert_eq!(
            durable
                .iter()
                .filter(|row| row.uuid == id.as_uuid().to_string())
                .count(),
            1,
            "each accepted listing has one durable source row"
        );
    }
}

#[test]
fn batched_listing_survives_blocked_initial_prompt() {
    run_with_large_stack(|| check_listing_rejected_prompt(false, false));
}

#[test]
fn streaming_listing_survives_blocked_initial_prompt() {
    run_with_large_stack(|| check_listing_rejected_prompt(true, false));
}

#[test]
fn batched_listing_survives_preassembly_cancellation() {
    run_with_large_stack(|| check_listing_rejected_prompt(false, true));
}

#[test]
fn streaming_listing_survives_preassembly_cancellation() {
    run_with_large_stack(|| check_listing_rejected_prompt(true, true));
}

#[test]
fn listing_admission_is_fenced_by_session_generation_and_membership() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let (orchestrator, _, _) = listing_orchestrator(
            root.path(),
            &root.path().join("generation.jsonl"),
            Arc::new(PosixFileSystem::new(root.path().to_owned())),
            noop_hook_executor(),
        );
        let prompt_id = MessageId::new();
        REQUEST_CONTEXT
            .scope(request_prompt_context(prompt_id), async {
                REQUEST_CONTEXT.with(|context| {
                    context
                        .ingress_batch
                        .lock()
                        .unwrap()
                        .stage_listing_membership = true;
                });
                let row = orchestrator
                    .skill_listing_prompt_ingress_attachment()
                    .await
                    .unwrap()
                    .expect("real skill delta is staged");
                let agent_row = orchestrator
                    .agent_listing_prompt_ingress_attachment()
                    .await
                    .expect("real built-in agent delta is staged");
                let old = take_prompt_listing_memberships(&[row, agent_row]);
                assert_eq!(old.len(), 2);
                assert!(orchestrator
                    .prompt_runtime
                    .sent_skill_names
                    .lock()
                    .await
                    .is_empty());
                orchestrator
                    .prompt_runtime
                    .prepare_session_scoped_reset()
                    .await;
                let current = vec!["new-session-only".encode_utf16().collect::<Vec<_>>()]
                    .into_iter()
                    .collect();
                *orchestrator.prompt_runtime.sent_skill_names.lock().await = current;
                orchestrator
                    .prompt_runtime
                    .sent_agent_names
                    .lock()
                    .await
                    .insert("new-session-agent".to_owned());
                for membership in old {
                    orchestrator
                        .commit_prompt_listing_membership(membership)
                        .await;
                }
                assert!(orchestrator
                    .prompt_runtime
                    .sent_skill_names
                    .lock()
                    .await
                    .contains(&"new-session-only".encode_utf16().collect::<Vec<_>>()));
                assert!(orchestrator
                    .prompt_runtime
                    .sent_agent_names
                    .lock()
                    .await
                    .contains("new-session-agent"));
                orchestrator
                    .commit_prompt_listing_membership(PromptListingMembership::Skills {
                        generation: orchestrator
                            .prompt_runtime
                            .listing_membership_generation
                            .load(Ordering::Acquire),
                        before: Default::default(),
                        after: Default::default(),
                    })
                    .await;
                assert!(
                    orchestrator
                        .prompt_runtime
                        .sent_skill_names
                        .lock()
                        .await
                        .contains(&"new-session-only".encode_utf16().collect::<Vec<_>>()),
                    "an outdated same-generation snapshot cannot overwrite newer membership"
                );
            })
            .await;
    });
}

/// Pause the real Read tool's restoration I/O seam after the compactor has
/// drained the old read map. The opened handle and policy checks remain the
/// production FileReadTool implementation.
struct CompactReadGate {
    reader: Arc<tool_file::FileReadTool>,
    gate: Arc<OneShotGate>,
}

async fn assert_complete_live_cold_context(
    orchestrator: &ConversationOrchestrator,
    cwd: &Path,
    transcript: &Path,
    fs: Arc<dyn FileSystem>,
) -> session::jsonl::LoadedTranscript {
    let (session_id, mut live) = {
        let state = orchestrator.session.lock().await;
        (state.session_id, state.model_context_history())
    };
    let loaded = JsonlReader::new(transcript.to_owned(), fs)
        .read_routed()
        .await
        .unwrap();
    let (active_branch, branch_session) =
        session::jsonl::build_conversation_chain(&loaded, &session_id.as_uuid().to_string());
    assert_eq!(branch_session, session_id.as_uuid().to_string());
    let resumed = crate::resume::state_from_messages(session_id.as_uuid(), &active_branch);
    let mut cold_history = resumed.model_context_history();
    let describe = |messages: &[ConversationMessage]| {
        messages
            .iter()
            .map(|row| {
                (
                    row.id(),
                    row.text_content().chars().take(120).collect::<String>(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        cold_history.iter().map(ConversationMessage::id).collect::<Vec<_>>(),
        live.iter().map(ConversationMessage::id).collect::<Vec<_>>(),
        "the complete live active history must match the durable branch; live={:?}; cold={:?}; graph={:?}",
        describe(&live), describe(&cold_history),
        active_branch.iter().map(|row| (&row.uuid, &row.parent_uuid, &row.message_type,
            row.attachment_projection.as_ref().and_then(|source| source.string_at("/type"))
        )).collect::<Vec<_>>()
    );
    let cold = ConversationOrchestrator::new(
        crate::OrchestratorConfig {
            model: "claude-opus-4-7".into(),
            ..Default::default()
        },
        Arc::new(MockApiClient::new(Vec::new())),
        orchestrator.tools.clone(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    );
    *cold.session.lock().await = resumed;
    cold.restore_resume_runtime_metadata(&active_branch).await;
    // Run the real outgoing reconstruction/Native attachment projection on
    // every active row with each owner's own attachment metadata. These are
    // complete vectors, not an intersection or a test-side canonical sort.
    let context = crate::conversation::PreparedContextAnnouncements::default();
    orchestrator
        .reattach_outgoing_context(
            &mut live,
            None,
            None,
            &mut Vec::new(),
            &mut Vec::new(),
            &context,
            false,
        )
        .await;
    cold.reattach_outgoing_context(
        &mut cold_history,
        None,
        None,
        &mut Vec::new(),
        &mut Vec::new(),
        &context,
        false,
    )
    .await;
    assert_eq!(
        cold_history.iter().map(ConversationMessage::id).collect::<Vec<_>>(),
        live.iter().map(ConversationMessage::id).collect::<Vec<_>>(),
        "the complete live/cold outgoing provider projections must retain identical UUID order; live={:?}; cold={:?}",
        describe(&live), describe(&cold_history)
    );
    loaded
}

#[async_trait]
impl Tool for CompactReadGate {
    fn name(&self) -> &str {
        self.reader.name()
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        self.reader.input_schema()
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        self.reader.input_repair_metadata()
    }
    fn is_enabled(&self, context: &ToolStaticContext) -> bool {
        self.reader.is_enabled(context)
    }
    fn max_result_size_chars(&self) -> usize {
        self.reader.max_result_size_chars()
    }
    fn is_concurrency_safe(&self, input: &Utf16JsonProjection) -> bool {
        self.reader.is_concurrency_safe(input)
    }
    fn is_read_only(&self, input: &Utf16JsonProjection) -> bool {
        self.reader.is_read_only(input)
    }
    fn read_state_io_path(&self, path: &Path) -> Result<std::path::PathBuf, ToolError> {
        self.reader.read_state_io_path(path)
    }
    async fn open_file_for_compaction(
        &self,
        path: &Path,
    ) -> Result<Option<std::fs::File>, ToolError> {
        self.gate.pause_once().await;
        self.reader.open_file_for_compaction(path).await
    }
    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        context: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        self.reader.validate_input(input, context).await
    }
    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        context: &ToolUseContext,
    ) -> permission::PermissionResult {
        self.reader.check_permissions(input, context).await
    }
    async fn description(
        &self,
        input: &Utf16JsonProjection,
        options: &DescriptionOptions,
    ) -> String {
        self.reader.description(input, options).await
    }
    async fn prompt(&self, options: &PromptOptions) -> Utf16JsonProjection {
        self.reader.prompt(options).await
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        context: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.reader.call(input, context, progress).await
    }
}

async fn check_auto_compact_commit_cancel(streaming: bool, during_restore_io: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let path = cwd.join("read-before-auto-compact.txt");
    std::fs::write(&path, "real Read baseline retained across compaction\n").unwrap();
    let transcript = cwd.join("auto-compact-cancel.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let gate = Arc::new(OneShotGate::default());
    let hooks = if during_restore_io {
        noop_hook_executor()
    } else {
        hooks_with(
            HookEventType::PostCompact,
            Arc::new(PostCompactGate(gate.clone())),
        )
        .await
    };
    let (mut orchestrator, api, streaming_api) =
        listing_orchestrator(&cwd, &transcript, fs.clone(), hooks);
    let reads = tool_api::read_file_state::new_read_file_state_map();
    let mut context = tool_api::test_support::ctx_for_file_tools(
        fs.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    context.read_file_state = reads.clone();
    let reader = Arc::new(tool_file::FileReadTool::new(context));
    reader
        .call(
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path": path})).unwrap(),
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    assert!(tool_api::read_file_state::get(&reads, &path).is_some());
    let mut tools = ToolRegistry::new();
    if during_restore_io {
        tools.register_builtin(Arc::new(CompactReadGate {
            reader,
            gate: gate.clone(),
        }));
    } else {
        tools.register_builtin(reader);
    }
    orchestrator.tools = Arc::new(tools);
    let orchestrator = ConversationOrchestrator::into_shared(with_scripted_compactor(
        orchestrator.with_read_state_map(reads.clone()),
        "committed automatic lifecycle summary",
    ));
    bind_test_durable_transcript_target(&orchestrator, &cwd, &cwd.join("state"), &transcript);
    for index in 0..60 {
        let row = if index % 2 == 0 {
            ConversationMessage::user(
                MessageId::new(),
                format!("old turn {index} {}", "filler ".repeat(30)),
            )
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: format!("old answer {index} {}", "detail ".repeat(30)),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                per_turn_effort: None,
                resumable: None,
            }
        };
        orchestrator.session.lock().await.history.push(row.clone());
        orchestrator.persist_message_to_jsonl(&row).await;
    }
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = run(
        &orchestrator,
        streaming,
        "compact then cancel",
        Some(cancel.clone()),
    );
    tokio::pin!(first);
    tokio::select! {
        () = gate.entered.notified() => {}
        result = &mut first => panic!("automatic compaction did not reach its commit gate: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(10)) => panic!("automatic compaction gate timed out"),
    }
    cancel.cancel();
    let prematurely_returned = tokio::time::timeout(Duration::from_millis(20), &mut first).await;
    gate.release.notify_one();
    let (completed, escaped_commit) = match prematurely_returned {
        Ok(result) => (result, true),
        Err(_) => (
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .unwrap(),
            false,
        ),
    };
    assert!(
        !escaped_commit,
        "cancellation dropped an active post-compact commit"
    );
    assert_eq!(completed.unwrap(), RunOutcome::Cancelled);
    assert!(captured(&api, &streaming_api, streaming).await.is_empty());
    {
        let state = orchestrator.session.lock().await;
        assert!(state.history.iter().any(|row| matches!(row,
            ConversationMessage::System { subtype: Some(subtype), .. } if subtype == "compact_boundary"
        )));
    }
    let loaded = assert_complete_live_cold_context(&orchestrator, &cwd, &transcript, fs).await;
    let interruption = orchestrator
        .session
        .lock()
        .await
        .model_context_history()
        .into_iter()
        .find(|row| row.text_content() == "[Request interrupted by user]")
        .expect("the cancelled driver publishes its interruption marker")
        .id();
    let nar = loaded
        .messages_in_order
        .iter()
        .filter(|row| {
            row.attachment_projection
                .as_ref()
                .and_then(|source| source.string_at("/type"))
                .is_some_and(|kind| {
                    matches!(kind, "environment" | "model" | "total_tokens_reminder")
                })
        })
        .collect::<Vec<_>>();
    if !nar.is_empty() {
        assert!(nar.iter().any(|row| row
            .attachment_projection
            .as_ref()
            .unwrap()
            .string_at("/type")
            == Some("environment")));
        assert!(nar.iter().any(|row| row
            .attachment_projection
            .as_ref()
            .unwrap()
            .string_at("/type")
            == Some("model")));
    }
    let mut parent = interruption.as_uuid().to_string();
    for row in nar {
        assert_eq!(row.parent_uuid.as_deref(), Some(parent.as_str()),
            "a cancelled pre-admission NAr batch follows the interruption marker in the durable parent graph");
        parent = row.uuid.clone();
    }
    assert!(
        loaded.messages_in_order.iter().any(|row| row
            .attachment_projection
            .as_ref()
            .is_some_and(|source| source.string_at("/type") == Some("file"))),
        "the Read restoration sidecar must be durable before cancellation returns"
    );
    assert!(tool_api::read_file_state::get(&reads, &path).is_some());
    assert_eq!(
        run(
            &orchestrator,
            streaming,
            "continue after cancellation",
            None
        )
        .await
        .unwrap(),
        RunOutcome::End
    );
    let calls = captured(&api, &streaming_api, streaming).await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().any(|row| row
        .text_content()
        .contains("committed automatic lifecycle summary")));
    assert!(calls[0]
        .iter()
        .any(|row| row.text_content().contains("real Read baseline retained")));
}

#[test]
fn batched_auto_compact_cancellation_finishes_restore_io_commit() {
    run_with_large_stack(|| check_auto_compact_commit_cancel(false, true));
}

#[test]
fn streaming_auto_compact_cancellation_finishes_restore_io_commit() {
    run_with_large_stack(|| check_auto_compact_commit_cancel(true, true));
}

#[test]
fn batched_auto_compact_cancellation_finishes_post_compact_hook_commit() {
    run_with_large_stack(|| check_auto_compact_commit_cancel(false, false));
}

#[test]
fn streaming_auto_compact_cancellation_finishes_post_compact_hook_commit() {
    run_with_large_stack(|| check_auto_compact_commit_cancel(true, false));
}

async fn check_cancel_after_ingress_admission(streaming: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("cancel-after-ingress-admission.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let (orchestrator, api, streaming_api) =
        listing_orchestrator(&cwd, &transcript, fs.clone(), noop_hook_executor());
    let orchestrator = ConversationOrchestrator::into_shared(orchestrator);
    bind_test_durable_transcript_target(&orchestrator, &cwd, &cwd.join("state"), &transcript);
    let gate = Arc::new(PromptIngressAppendTestGate::default());
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = PROMPT_INGRESS_ADMISSION_TEST_GATE.scope(
        gate.clone(),
        run(
            &orchestrator,
            streaming,
            "cancel after catalog admission",
            Some(cancel.clone()),
        ),
    );
    tokio::pin!(first);
    tokio::select! {
        () = gate.wait_until_entered() => {}
        result = &mut first => panic!("initial admission did not reach its barrier: {result:?}"),
    }
    // This is before append_started: history has the real Skill and Agent
    // rows, while the first durable attachment append has not yet begun.
    assert_eq!(
        listing_rows(&orchestrator, &orchestrator.session.lock().await.history).len(),
        2
    );
    cancel.cancel();
    let early = tokio::time::timeout(Duration::from_millis(20), &mut first).await;
    gate.resume();
    let (result, escaped_commit) = match early {
        Ok(result) => (result, true),
        Err(_) => (
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .unwrap(),
            false,
        ),
    };
    assert!(
        !escaped_commit,
        "cancel dropped the live-admitted batch before its first write"
    );
    assert_eq!(result.unwrap(), RunOutcome::Cancelled);
    assert!(captured(&api, &streaming_api, streaming).await.is_empty());
    let loaded = assert_complete_live_cold_context(&orchestrator, &cwd, &transcript, fs).await;
    let interruption = loaded
        .messages_in_order
        .iter()
        .position(|row| {
            row.message.string_at("/content") == Some("[Request interrupted by user]")
                || row.message.string_at("/content/0/text") == Some("[Request interrupted by user]")
        })
        .expect("cancelled driver writes its interruption marker");
    for (position, row) in loaded.messages_in_order.iter().enumerate() {
        if row
            .attachment_projection
            .as_ref()
            .and_then(|source| source.string_at("/type"))
            .is_some_and(|kind| matches!(kind, "skill_listing" | "agent_listing_delta"))
        {
            assert!(
                position < interruption,
                "once admission starts, its listing commits before interruption"
            );
        }
    }
    assert_eq!(
        run(
            &orchestrator,
            streaming,
            "continue after admitted cancellation",
            None
        )
        .await
        .unwrap(),
        RunOutcome::End
    );
    let calls = captured(&api, &streaming_api, streaming).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(listing_rows(&orchestrator, &calls[0]).len(), 2);
}

#[test]
fn batched_cancel_after_ingress_admission_finishes_before_interruption() {
    run_with_large_stack(|| check_cancel_after_ingress_admission(false));
}

#[test]
fn streaming_cancel_after_ingress_admission_finishes_before_interruption() {
    run_with_large_stack(|| check_cancel_after_ingress_admission(true));
}

/// A deterministic in-process MCP server. The real registry handshake/catalog
/// publication and production MCP tool builder are used; no subprocess or
/// provider is involved.
struct SteeringMcpTransport {
    connection_id: lingxi_core::types::McpConnectionId,
}

#[async_trait]
impl lingxi_core::host::McpTransport for SteeringMcpTransport {
    async fn connect(
        &self,
        _: &lingxi_core::host::McpTransportSpec,
    ) -> Result<lingxi_core::host::McpRawConnection, lingxi_core::host::McpError> {
        Ok(lingxi_core::host::McpRawConnection {
            connection_id: self.connection_id,
        })
    }
    async fn initialize(
        &self,
        _: &lingxi_core::host::McpRawConnection,
    ) -> Result<lingxi_core::host::ServerCapabilitiesDto, lingxi_core::host::McpError> {
        Ok(lingxi_core::host::ServerCapabilitiesDto {
            tools: true,
            ..Default::default()
        })
    }
    fn server_metadata(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Option<lingxi_core::host::McpServerMetadataDto> {
        (id == self.connection_id).then(|| lingxi_core::host::McpServerMetadataDto {
            instructions: Some("Read the newly connected steering index first.".into()),
            ..Default::default()
        })
    }
    async fn list_tools(
        &self,
        _: &lingxi_core::host::McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpToolDto>, lingxi_core::host::McpError> {
        Ok(vec![lingxi_core::host::McpToolDto {
            server_name: String::new(),
            tool_name: "inspect".into(),
            wire_tool_name: None,
            description: "Inspect the newly connected steering index".into(),
            input_schema: Utf16JsonProjection::parse(r#"{"type":"object","properties":{"index":{"type":"string"}},"required":["index"]}"#).unwrap(),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: String::new(),
            search_hint: None,
            always_load: Some(false),
            requires_user_interaction: false,
        }])
    }
    async fn list_resources(
        &self,
        _: &lingxi_core::host::McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpResourceDto>, lingxi_core::host::McpError> {
        Ok(Vec::new())
    }
    async fn list_prompts(
        &self,
        _: &lingxi_core::host::McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpPromptDto>, lingxi_core::host::McpError> {
        Ok(Vec::new())
    }
    async fn call_tool(
        &self,
        _: &lingxi_core::host::McpRawConnection,
        _: &str,
        _: Utf16JsonProjection,
    ) -> Result<lingxi_core::host::McpToolResultDto, lingxi_core::host::McpError> {
        unreachable!("the fixture only prepares catalogs")
    }
    async fn read_resource(
        &self,
        _: &lingxi_core::host::McpRawConnection,
        _: &Utf16JsonProjection,
    ) -> Result<lingxi_core::host::McpResourceContentDto, lingxi_core::host::McpError> {
        unreachable!("resources are disabled")
    }
    async fn ping(
        &self,
        _: lingxi_core::types::McpConnectionId,
    ) -> Result<(), lingxi_core::host::McpError> {
        Ok(())
    }
    async fn notifications(
        &self,
        _: &lingxi_core::host::McpRawConnection,
    ) -> Result<lingxi_core::host::McpNotificationStream, lingxi_core::host::McpError> {
        unreachable!("in-process catalogs do not subscribe to RPC notifications")
    }
    async fn handle_elicitation(
        &self,
        _: &lingxi_core::host::McpRawConnection,
        _: lingxi_core::host::ElicitRequestDto,
    ) -> Result<lingxi_core::host::ElicitResultDto, lingxi_core::host::McpError> {
        unreachable!("elicitation is disabled")
    }
    async fn disconnect(
        &self,
        _: lingxi_core::types::McpConnectionId,
    ) -> Result<(), lingxi_core::host::McpError> {
        Ok(())
    }
    fn supported_transports(&self) -> Vec<lingxi_core::host::McpTransportKind> {
        vec![lingxi_core::host::McpTransportKind::InProcess]
    }
}

async fn check_steering_refreshes_live_mcp_catalog(path: ModelCallPath) {
    const TOOL: &str = "mcp__steering__inspect";
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("steering-catalog.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let api = Arc::new(MockApiClient::new(Vec::new()));
    api.set_native_request_route(Some(llm_runtime::ProtocolFamily::AnthropicMessages), true);
    api.set_native_mid_system_supported(true);
    api.set_native_mid_tool_change_supported(Some(true));
    api.set_native_tool_change_feature_gates(true, true);
    let mut tools = ToolRegistry::new();
    tools.set_deferral(Arc::new(tool_api::defer::DeferralState::new(
        tool_api::defer::ToolSearchMode::Enabled,
        false,
    )));
    tools.register_builtin(Arc::new(SkillListingGateTool { name: "ToolSearch" }));
    let tools = Arc::new(tools);
    let registry = Arc::new(mcp::McpRegistry::new(Arc::new(SteeringMcpTransport {
        connection_id: lingxi_core::types::McpConnectionId::new(),
    })));
    let mut orchestrator =
        orchestrator_with_api_and_tools(&cwd, &transcript, fs.clone(), api, tools.clone())
            .with_mcp_registry(registry.clone())
            .with_native_tool_pool_refresh_callback_available(Some(true));
    // Native's ordinary ToolSearch path records surfaced names without an
    // inline schema. A real tool-wide deny keeps ToolSearch registered while
    // filtering it from this request, exercising the pfr path whose newly
    // published schemas are delivered as API-system tool additions.
    orchestrator.perms = Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(permission::PermissionPolicy::from_rules(
            permission::PermissionMode::Default,
            vec![permission::PermissionRule {
                value: permission::PermissionRuleValue {
                    tool_name: "ToolSearch".into(),
                    rule_content: None,
                    original_rule: None,
                },
                behavior: permission::PermissionBehavior::Deny,
                source: permission::PermissionRuleSource::Session,
            }],
        )),
        Arc::new(NoOpPermissionGate),
    ));
    bind_test_durable_transcript_target(&orchestrator, &cwd, &cwd.join("state"), &transcript);
    let prompt_id = MessageId::new();
    let prompt = ConversationMessage::user(prompt_id, "initial catalog snapshot".into());
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(prompt.clone());
    orchestrator.persist_message_to_jsonl(&prompt).await;
    REQUEST_CONTEXT
        .scope(request_prompt_context(prompt_id), async {
            persist_prompt_ingress_after_user(
                &orchestrator,
                prompt_id,
                PromptIngressKind::RegularUser,
            )
            .await
            .unwrap();
            let initial = orchestrator
                .prepare_turn_step(path, None, true, true, None)
                .await
                .unwrap();
            assert!(prompt_ingress_batch_finalized());
            assert!(prompt_ingress_producer_rows("mcp_instructions_delta")
                .unwrap()
                .is_empty());
            assert!(!initial.wire_tools.iter().any(|tool| tool.name == TOOL));
            assert!(!initial.wire_tools.iter().any(|tool| tool.name == "ToolSearch"),
                "the real permission gate filters ToolSearch from the request");
            assert!(!initial.snapshot.iter().any(|row| row
                .text_content()
                .contains("newly connected steering index")));
            let prior_assistant = ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: "initial model response before human steering".into(),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                per_turn_effort: None,
                resumable: None,
            };
            let prior_assistant_id = prior_assistant.id();
            orchestrator.session.lock().await.history.push(prior_assistant.clone());
            orchestrator.persist_message_to_jsonl(&prior_assistant).await;

            // Complete a real server handshake after the initial NAr was finalized,
            // then publish the production MCP tool entries to the same live pool.
            registry
                .connect(mcp::McpServerConfig {
                    name: "steering".into(),
                    spec: lingxi_core::host::McpTransportSpec::InProcess {
                        registry_key: "steering-fixture".into(),
                    },
                    scope: mcp::ConfigScope::Dynamic,
                    disabled: false,
                    timeout_ms: None,
                    always_load: false,
                    discovery_cache: None,
                    tools: Vec::new(),
                    tool_permissions: Default::default(),
                    config_error: None,
                    metadata: Default::default(),
                })
                .await
                .unwrap();
            let mut context = tool_api::test_support::ctx_for_file_tools(
                fs.clone(),
                Arc::new(telemetry::AnalyticsBus::new()),
                vec![cwd.clone()],
            );
            context.mcp_registry = Some(registry.clone());
            tools.replace_mcp_tools(tool_mcp::build_registered_mcp_tools(&registry, context).await);
            orchestrator
                .inject_user_text("steering with the newly connected server", false)
                .await;
            let steering_id = orchestrator
                .session
                .lock()
                .await
                .history
                .iter()
                .find(|row| row.text_content() == "steering with the newly connected server")
                .unwrap()
                .id();
            let prepared = orchestrator
                .prepare_turn_step(path, None, true, true, None)
                .await
                .unwrap();
            let additions = prepared.snapshot.iter().filter_map(|row| {
                let ConversationMessage::System { api_system: Some(system), .. } = row else {
                    return None;
                };
                system.content.iter().find_map(|block| {
                    let ContentBlock::ProviderContent { protocol, value } = block else {
                        return None;
                    };
                    (protocol == "anthropic_messages"
                        && value.string_at("/type") == Some("tool_addition")
                        && value.string_at("/tool/type") == Some("tool_definition")
                        && value.string_at("/tool/definition/name") == Some(TOOL))
                        .then_some((row.id(), value))
                })
            }).collect::<Vec<_>>();
            assert_eq!(additions.len(), 1,
                "the actual provider request gets one new inline MCP schema; header={:?}; api_system={:?}; typed={:?}",
                prepared.wire_tools,
                prepared.snapshot.iter().filter(|row| matches!(row,
                    ConversationMessage::System { api_system: Some(_), .. }
                )).collect::<Vec<_>>(),
                orchestrator.transcript.model_reminder_attachments.lock().unwrap()
                    .values().filter(|source| source.string_at("/type")
                        .is_some_and(|kind| kind.starts_with("deferred_tools")))
                    .map(|source| source.to_json_string().unwrap()).collect::<Vec<_>>());
            let addition_id = additions[0].0;
            assert_eq!(
                additions[0].1.string_at("/tool/definition/input_schema/properties/index/type"),
                Some("string")
            );
            let steering_position = prepared
                .snapshot
                .iter()
                .position(|row| row.id() == steering_id)
                .unwrap();
            let instructions = prepared
                .snapshot
                .iter()
                .enumerate()
                .filter(|(_, row)| {
                    row.text_content()
                        .contains("Read the newly connected steering index first.")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                instructions.len(),
                1,
                "fresh schemas must carry the matching server instructions"
            );
            assert!(
                instructions[0].0 < steering_position
                    && instructions[0].0 > prepared.snapshot.iter()
                        .position(|row| row.id() == prior_assistant_id).unwrap(),
                "Native puts fresh instructions in the later human prompt's boundary group"
            );
            let instruction_id = instructions[0].1.id();
            let relevant_ids = [prompt_id, prior_assistant_id, instruction_id, addition_id, steering_id];
            let prepared_order = prepared.snapshot.iter().map(ConversationMessage::id)
                .filter(|id| relevant_ids.contains(id)).collect::<Vec<_>>();
            assert_eq!(prepared_order, relevant_ids,
                "new instructions and schema additions precede the later human prompt without moving the initial prompt");
            let durable_before = JsonlReader::new(transcript.clone(), fs.clone())
                .read_all()
                .await
                .unwrap();
            let fresh_delta = durable_before
                .iter()
                .find(|row| {
                    row.attachment_projection.as_ref().is_some_and(|source| {
                        source.string_at("/type") == Some("deferred_tools_delta")
                            && source.to_json_string().unwrap().contains(TOOL)
                    })
                })
                .expect("the new searchable tool has a real durable deferred delta");
            let delta_id = fresh_delta.uuid.clone();
            assert!(orchestrator.session.lock().await.history.iter()
                .any(|row| row.id().as_uuid().to_string() == delta_id),
                "fresh deferred deltas must be admitted to hot history as well as JSONL");
            assert_complete_live_cold_context(&orchestrator, &cwd, &transcript, fs.clone()).await;
            assert_eq!(
                durable_before
                    .iter()
                    .filter(|row| row.uuid == instruction_id.as_uuid().to_string())
                    .count(),
                1
            );
            for row in &initial.snapshot {
                if prepared
                    .snapshot
                    .iter()
                    .any(|candidate| candidate.id() == row.id())
                {
                    assert_eq!(
                        prepared
                            .snapshot
                            .iter()
                            .filter(|candidate| candidate.id() == row.id())
                            .count(),
                        1
                    );
                }
            }

            // This is the production same-step physical retry path: rebuild from
            // durable history, then reattach the already-prepared suffix. It does
            // not invoke a new producer or issue replacement row identities.
            let mut rebuilt = orchestrator.session.lock().await.model_context_history();
            let mut reminders = prepared.turn_reminders.clone();
            let mut guarded = prepared.guarded_async_hook_reminders.clone();
            orchestrator
                .reattach_outgoing_context(
                    &mut rebuilt,
                    prepared.deferred_reminder.as_ref(),
                    prepared.date_change_reminder.as_ref(),
                    &mut reminders,
                    &mut guarded,
                    &prepared.context_announcements,
                    false,
                )
                .await;
            assert_eq!(
                rebuilt
                    .iter()
                    .filter(|row| row.id() == instruction_id)
                    .count(),
                1
            );
            assert_eq!(rebuilt.iter().filter(|row| row.id() == addition_id).count(), 1);
            assert_eq!(rebuilt.iter().map(ConversationMessage::id)
                .filter(|id| relevant_ids.contains(id)).collect::<Vec<_>>(), prepared_order,
                "physical retry retains the exact prepared instruction/schema/human order");
            let durable_after = JsonlReader::new(transcript.clone(), fs.clone())
                .read_all()
                .await
                .unwrap();
            assert_eq!(
                durable_before.len(),
                durable_after.len(),
                "physical retry reuses accepted rows"
            );
            assert_eq!(
                durable_after
                    .iter()
                    .filter(|row| row.uuid == delta_id)
                    .count(),
                1
            );
            let later = orchestrator.prepare_turn_step(path, None, false, false, None).await.unwrap();
            assert_eq!(later.snapshot.iter().filter(|row| row.id().as_uuid().to_string() == delta_id).count(), 1,
                "the next model step retains the same delta UUID exactly once");
            assert_complete_live_cold_context(&orchestrator, &cwd, &transcript, fs.clone()).await;
            assert_eq!(
                durable_after
                    .iter()
                    .filter(|row| row.uuid == instruction_id.as_uuid().to_string())
                    .count(),
                1
            );
        })
        .await;
}

#[test]
fn batched_steering_refreshes_mcp_instructions_and_deferred_catalog() {
    run_with_large_stack(|| check_steering_refreshes_live_mcp_catalog(ModelCallPath::Batched));
}

#[test]
fn streaming_steering_refreshes_mcp_instructions_and_deferred_catalog() {
    run_with_large_stack(|| check_steering_refreshes_live_mcp_catalog(ModelCallPath::Streaming));
}

async fn check_skill_withdrawal_replay(streaming: bool, cold: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("skill-withdrawal.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let (orch, _, _) = listing_orchestrator(&cwd, &transcript, fs.clone(), noop_hook_executor());
    let provider = Arc::new(HotListing(AtomicBool::new(true)));
    let orch = ConversationOrchestrator::into_shared(orch.with_skill_listing(provider.clone()));
    bind_test_durable_transcript_target(&orch, &cwd, &cwd.join("state"), &transcript);
    run(&orch, streaming, "catalog on", None).await.unwrap();
    provider.0.store(false, Ordering::Release);
    run(&orch, streaming, "catalog off", None).await.unwrap();
    let name = "late-skill".encode_utf16().collect::<Vec<_>>();
    assert!(!orch
        .prompt_runtime
        .sent_skill_names
        .lock()
        .await
        .contains(&name));
    let target = if cold {
        let loaded = JsonlReader::new(transcript.clone(), fs.clone())
            .read_routed()
            .await
            .unwrap();
        let id = orch.session.lock().await.session_id;
        let (chain, _) =
            session::jsonl::build_conversation_chain(&loaded, &id.as_uuid().to_string());
        let (resumed, _, _) = listing_orchestrator(&cwd, &transcript, fs, noop_hook_executor());
        let resumed =
            ConversationOrchestrator::into_shared(resumed.with_skill_listing(provider.clone()));
        *resumed.session.lock().await = crate::resume::state_from_messages(id.as_uuid(), &chain);
        resumed.restore_resume_runtime_metadata(&chain).await;
        resumed
    } else {
        orch.restore_hot_resume_prompt_state().await;
        orch
    };
    assert!(!target
        .prompt_runtime
        .sent_skill_names
        .lock()
        .await
        .contains(&name));
    provider.0.store(true, Ordering::Release);
    scope_if_missing(&target, async {
        let reminder = target
            .skill_listing_reminder_message()
            .await
            .unwrap()
            .expect("reactivation must be announced");
        assert!(reminder.text_content().contains("late-skill"));
        target.acknowledge_step_reminders(&[reminder]).await;
        assert!(target
            .skill_listing_reminder_message()
            .await
            .unwrap()
            .is_none());
        Ok(())
    })
    .await
    .unwrap();
}
#[test]
fn batched_skill_withdrawal_replays_in_order_before_reactivation_on_hot_and_cold_resume() {
    run_with_large_stack(|| async {
        for cold in [false, true] {
            check_skill_withdrawal_replay(false, cold).await;
        }
    });
}
#[test]
fn streaming_skill_withdrawal_replays_in_order_before_reactivation_on_hot_and_cold_resume() {
    run_with_large_stack(|| async {
        for cold in [false, true] {
            check_skill_withdrawal_replay(true, cold).await;
        }
    });
}

struct OverflowOnceApi {
    inner: Arc<MockApiClient>,
    first: AtomicBool,
}
#[async_trait]
impl crate::OrchestratorApiClient for OverflowOnceApi {
    async fn messages_create(
        &self,
        request: crate::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        let response =
            crate::OrchestratorApiClient::messages_create(self.inner.as_ref(), request).await;
        if self.first.swap(false, Ordering::SeqCst) {
            Err(llm_runtime::LlmError::ContextOverflow { token_gap: 100 })
        } else {
            response
        }
    }
}

async fn check_reactive_compact_keeps_delivered_prefetch_acks(streaming: bool) {
    use crate::conversation::pending_reminders::Acknowledgement;
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("reactive-prefetch.jsonl");
    let api = Arc::new(MockApiClient::new(replies(4)));
    let batched = Arc::new(OverflowOnceApi {
        inner: api.clone(),
        first: AtomicBool::new(true),
    });
    let stream = Arc::new(MockStreamingApiClient::with_open_error(
        llm_runtime::LlmError::ContextOverflow { token_gap: 100 },
        streams(3),
    ));
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let mut orch = with_scripted_compactor(
        ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig {
                model: "claude-opus-4-7".into(),
                ..Default::default()
            },
            batched,
            stream.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_jsonl_writer(Arc::new(JsonlWriter::new(transcript.clone(), fs))),
        "reactive summary",
    );
    let engine = Arc::try_unwrap(orch.compaction_runtime.compaction.take().unwrap())
        .unwrap_or_else(|_| panic!("fixture compactor is unexpectedly shared"));
    orch.compaction_runtime.compaction = Some(Arc::new(engine.with_model_derived_threshold()));
    bind_test_durable_transcript_target(&orch, &cwd, &cwd.join("state"), &transcript);
    for index in 0..6 {
        orch.append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            format!("prior history {index} ").repeat(20),
        ))
        .await;
        orch.append_external_history_message(ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: format!("prior answer {index}"),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            per_turn_effort: None,
            resumable: None,
        })
        .await;
    }
    let orch = ConversationOrchestrator::into_shared(orch);
    let memory = memory::surfacing::SurfacedMemory {
        path: cwd.join("relevant.md"),
        content: "REACTIVE_MEMORY".into(),
        age_days: 0,
        mtime: std::time::SystemTime::UNIX_EPOCH,
    };
    std::fs::write(&memory.path, &memory.content).unwrap();
    let skill = skill::DiscoveredSkill {
        name: "reactive-skill".into(),
        description: "REACTIVE_SKILL".into(),
        short_id: None,
    };
    let stage = || {
        orch.retain_pending_step_reminder(
            "relevant_memory",
            ConversationMessage::user_meta(MessageId::new(), "REACTIVE_MEMORY".into()),
            Acknowledgement::Memory(memory.clone()),
        );
        orch.retain_pending_step_reminder(
            "skill_discovery",
            ConversationMessage::user_meta(MessageId::new(), "REACTIVE_SKILL".into()),
            Acknowledgement::Skills(vec![skill.clone()]),
        );
    };
    stage();
    run(&orch, streaming, "overflow and recover", None)
        .await
        .unwrap();
    assert!(orch.snapshot_history().await.iter().any(|row| matches!(row, ConversationMessage::System {subtype: Some(kind), ..} if kind == "compact_boundary")));
    if streaming {
        assert_eq!(stream.captured_calls().await.len(), 1);
    }
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 2);
    let has_boundary = |rows: &[ConversationMessage]| {
        rows.iter().any(|row| matches!(row, ConversationMessage::System {subtype: Some(kind), ..} if kind == "compact_boundary"))
    };
    assert!(!has_boundary(&calls[0]));
    assert!(has_boundary(&calls[1]));
    for marker in ["REACTIVE_MEMORY", "REACTIVE_SKILL"] {
        let ids = calls
            .iter()
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.text_content().contains(marker))
                    .map(ConversationMessage::id)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(ids[0].len(), 1, "{marker}");
        assert_eq!(
            ids[0], ids[1],
            "the physical retry keeps its original ordered reminder identity"
        );
    }
    assert!(orch
        .prompt_runtime
        .surfaced_memory_paths
        .lock()
        .await
        .contains(&memory.path));
    assert!(orch
        .prompt_runtime
        .surfaced_skill_names
        .lock()
        .await
        .contains(&skill.name));
    stage();
    run(&orch, streaming, "next model turn", None)
        .await
        .unwrap();
    let calls = captured(&api, &stream, streaming).await;
    assert!(calls
        .last()
        .unwrap()
        .iter()
        .all(|row| !row.text_content().contains("REACTIVE_MEMORY")
            && !row.text_content().contains("REACTIVE_SKILL")));
}
#[test]
fn batched_reactive_compaction_acknowledges_reattached_memory_and_skills_once() {
    run_with_large_stack(|| check_reactive_compact_keeps_delivered_prefetch_acks(false));
}
#[test]
fn streaming_reactive_compaction_acknowledges_reattached_memory_and_skills_once() {
    run_with_large_stack(|| check_reactive_compact_keeps_delivered_prefetch_acks(true));
}

async fn check_deferred_mod_keeps_raw_source(path: ModelCallPath, suppress: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let transcript = cwd.join("deferred-mod.jsonl");
    let module = cwd.join("deferred-mod.js");
    std::fs::write(&module, if suppress {
        "export function register(on) { on('prompt.attachment', {type:'deferred_tools_delta'}, () => ({text:null})); }"
    } else {
        "export function register(on) { on('prompt.attachment', {type:'deferred_tools_delta'}, ($,e,next) => next({...e,text:'MOD: '+e.text})); }"
    }).unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "deferred-mod",
        &cwd,
        &module,
        tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
    )
    .await
    .unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let (orch, _, _) = listing_orchestrator(&cwd, &transcript, fs.clone(), noop_hook_executor());
    let orch = orch.with_hook_registry(mod_registry(host));
    bind_test_durable_transcript_target(&orch, &cwd, &cwd.join("state"), &transcript);
    let prompt = ConversationMessage::user(MessageId::new(), "deferred source".into());
    orch.session.lock().await.history.push(prompt.clone());
    orch.persist_message_to_jsonl(&prompt).await;
    let body = "<system-reminder>\nDEFERRED_RAW_SOURCE\n</system-reminder>";
    let message = ConversationMessage::user_meta(MessageId::new(), body.into());
    let id = message.id();
    let row = PromptIngressAttachment {
        rendered: rendered_system_message(&message).unwrap(), message,
        payload: Utf16JsonProjection::parse(r#"{"type":"deferred_tools_delta","added":["mcp__fixture__tool"],"removed":[],"surfacedNames":[],"replacedNames":[]}"#).unwrap(),
    };
    REQUEST_CONTEXT
        .scope(request_prompt_context(prompt.id()), async {
            persist_prompt_ingress_after_user(&orch, prompt.id(), PromptIngressKind::RegularUser)
                .await
                .unwrap();
            cache_prompt_ingress_producer_rows_pending(&orch, "deferred_tools_delta", vec![row])
                .await;
            let mut step = orch
                .prepare_turn_step(path, None, true, true, None)
                .await
                .unwrap();
            let check = |rows: &[ConversationMessage]| {
                let selected = rows.iter().find(|row| row.id() == id);
                if suppress {
                    assert!(selected.is_none());
                } else {
                    assert_eq!(
                        selected.unwrap().text_content(),
                        "<system-reminder>\nMOD: DEFERRED_RAW_SOURCE\n</system-reminder>"
                    );
                }
            };
            check(&step.snapshot);
            let raw = orch.session.lock().await.model_context_history();
            assert_eq!(
                raw.iter()
                    .find(|row| row.id() == id)
                    .unwrap()
                    .text_content(),
                body
            );
            let mut retry = raw;
            orch.reattach_outgoing_context(
                &mut retry,
                step.deferred_reminder.as_ref(),
                step.date_change_reminder.as_ref(),
                &mut step.turn_reminders,
                &mut step.guarded_async_hook_reminders,
                &step.context_announcements,
                false,
            )
            .await;
            check(&retry);
            orch.restore_hot_resume_prompt_state().await;
            let mut hot = orch.session.lock().await.model_context_history();
            orch.reattach_outgoing_context(
                &mut hot,
                None,
                None,
                &mut Vec::new(),
                &mut Vec::new(),
                &Default::default(),
                false,
            )
            .await;
            check(&hot);
        })
        .await;
    let loaded = JsonlReader::new(transcript.clone(), fs.clone())
        .read_routed()
        .await
        .unwrap();
    let owner = orch.session.lock().await.session_id;
    let (chain, _) =
        session::jsonl::build_conversation_chain(&loaded, &owner.as_uuid().to_string());
    let (cold, _, _) = listing_orchestrator(&cwd, &transcript, fs, noop_hook_executor());
    *cold.session.lock().await = crate::resume::state_from_messages(owner.as_uuid(), &chain);
    cold.restore_resume_runtime_metadata(&chain).await;
    let mut restored = cold.session.lock().await.model_context_history();
    cold.reattach_outgoing_context(
        &mut restored,
        None,
        None,
        &mut Vec::new(),
        &mut Vec::new(),
        &Default::default(),
        false,
    )
    .await;
    assert_eq!(restored.iter().find(|row| row.id() == id).unwrap().text_content(), body,
        "cold resume without the Mod recovers pristine source, including a previously suppressed row");
}
#[test]
fn batched_deferred_mod_rewrites_only_request_copies_through_retry_hot_and_cold_resume() {
    run_with_large_stack(|| async {
        for suppress in [false, true] {
            check_deferred_mod_keeps_raw_source(ModelCallPath::Batched, suppress).await;
        }
    });
}
#[test]
fn streaming_deferred_mod_rewrites_only_request_copies_through_retry_hot_and_cold_resume() {
    run_with_large_stack(|| async {
        for suppress in [false, true] {
            check_deferred_mod_keeps_raw_source(ModelCallPath::Streaming, suppress).await;
        }
    });
}
