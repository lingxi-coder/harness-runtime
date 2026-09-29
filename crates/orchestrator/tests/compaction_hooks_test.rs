//! hooks compaction lifecycle — `PreCompact` / `PostCompact` fired around the
//! orchestrator's compaction path.
//!
//! Mirrors the TS firing semantics (`services/compact/compact.ts`):
//! `executePreCompactHooks` runs BEFORE the summary request carrying
//! `trigger = auto|manual` + `custom_instructions`; `executePostCompactHooks`
//! runs AFTER, carrying the produced `compact_summary`.
//!
//! Exercises the proactive auto-compact seam (`maybe_compact_before_call`),
//! which is the always-reachable automatic path: with a compactor wired at a
//! low threshold and an over-threshold history, running a turn fires
//! `PreCompact` (trigger=auto) before the pass and `PostCompact` after it.
//!
//! Scenarios:
//! 1. A registered `PreCompact` hook fires with `reason == "auto"` when the
//!    proactive trigger compacts.
//! 2. A registered `PostCompact` hook fires after, carrying a summary.
//! 3. A `PreCompact` hook that returns `Block` prevents the compact pass while
//!    the surrounding turn continues.
//! 4. Successful compaction reloads instructions with reason `compact`, then
//!    fires `SessionStart(source=compact)`, then `PostCompact`.
use llm_runtime::ContentBlock as LlmContentBlock;

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType, InstructionsLoadReason};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, OutputEvent, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{ConversationMessage, HookId, HttpRequest, HttpResponse, MessageId};
use orchestrator::prompt::MemoryFile;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;

// ---- unused HTTP / Runtime stubs (Builtin hooks never touch them) ----
struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

struct CompactSummaryClient;

#[async_trait]
impl sidequery::SideQueryClient for CompactSummaryClient {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>hook lifecycle summary</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

// ---- recording builtin handlers ----

/// Shared probe the test reads after the turn.
#[derive(Default)]
struct Probe {
    pre_fired: AtomicBool,
    post_fired: AtomicBool,
    pre_count: AtomicU32,
    /// The `reason` (= wire `trigger`) carried by the last `PreCompact` event.
    pre_trigger: Mutex<Option<String>>,
    /// The `summary` carried by the last `PostCompact` event.
    post_summary: Mutex<Option<String>>,
    /// The `trigger` (`manual`/`auto`) carried by the last `PostCompact` event.
    post_trigger: Mutex<Option<String>>,
    lifecycle_order: Mutex<Vec<String>>,
}

/// `PreCompact` hook that records the trigger. `fail` makes it return a non-zero
/// exit (best-effort failure); `block` makes it return a `Block` decision —
/// neither may abort compaction.
struct RecordPreCompact {
    probe: Arc<Probe>,
    fail: bool,
    block: bool,
}
#[async_trait]
impl BuiltinHookHandler for RecordPreCompact {
    fn id(&self) -> &str {
        "record-pre-compact"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::PreCompact { reason, .. } = event {
            self.probe.pre_fired.store(true, Ordering::SeqCst);
            self.probe.pre_count.fetch_add(1, Ordering::SeqCst);
            *self.probe.pre_trigger.lock().unwrap() = Some(reason.clone());
            self.probe
                .lifecycle_order
                .lock()
                .unwrap()
                .push("pre".into());
        }
        HookResult {
            outcome: if self.fail {
                HookOutcome::Error
            } else {
                HookOutcome::Success
            },
            stdout: String::new(),
            stderr: if self.fail {
                "boom".into()
            } else {
                String::new()
            },
            exit_code: if self.fail { Some(1) } else { None },
            response: if self.block {
                Some(HookResponse {
                    decision: Some(HookDecision::Block),
                    reason: Some("nope".into()),
                    ..Default::default()
                })
            } else {
                None
            },
        }
    }
}

/// `PostCompact` hook that records the summary.
struct RecordPostCompact {
    probe: Arc<Probe>,
}
#[async_trait]
impl BuiltinHookHandler for RecordPostCompact {
    fn id(&self) -> &str {
        "record-post-compact"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::PostCompact {
            summary, trigger, ..
        } = event
        {
            self.probe.post_fired.store(true, Ordering::SeqCst);
            *self.probe.post_summary.lock().unwrap() = Some(summary.clone());
            *self.probe.post_trigger.lock().unwrap() = Some(trigger.clone());
            self.probe
                .lifecycle_order
                .lock()
                .unwrap()
                .push("post".into());
        }
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

/// Records the two post-summary setup events Claude runs before PostCompact.
/// SessionStart contributes model-facing context so the test also proves that
/// its hook result survives in the compacted history.
struct RecordCompactSetup {
    probe: Arc<Probe>,
}

#[async_trait]
impl BuiltinHookHandler for RecordCompactSetup {
    fn id(&self) -> &str {
        "record-compact-setup"
    }

    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = match event {
            HookEvent::InstructionsLoaded { load_reason, .. } => {
                self.probe
                    .lifecycle_order
                    .lock()
                    .unwrap()
                    .push(format!("instructions:{load_reason:?}"));
                None
            }
            HookEvent::SessionStart { source, .. } => {
                self.probe
                    .lifecycle_order
                    .lock()
                    .unwrap()
                    .push(format!("session_start:{source}"));
                (source == "compact").then(|| HookResponse {
                    additional_context: Some("post-compact hook context".into()),
                    ..Default::default()
                })
            }
            _ => None,
        };
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response,
        }
    }
}

fn builtin_hook(handler_id: &str, event_type: HookEventType) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: handler_id.into(),
        events: vec![event_type],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: handler_id.into(),
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
    }
}

/// Build a hook executor with the supplied (handler, definition) pairs.
async fn hook_executor_with(
    entries: Vec<(Arc<dyn BuiltinHookHandler>, HookDefinition)>,
) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut w = registry.write().await;
        for (_, def) in &entries {
            w.register(def.clone());
        }
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    for (handler, _) in entries {
        exec.register_builtin(handler);
    }
    Arc::new(exec)
}

/// Build an orchestrator with a single `end_turn` mock response, a compactor at
/// `threshold`, and the provided hook executor.
fn make_orch(
    hooks: Arc<HookExecutorImpl>,
    threshold: u64,
) -> (Arc<ConversationOrchestrator>, Arc<MockOutputStream>) {
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".to_string(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output.clone(),
        memory,
        std::env::temp_dir(),
    )
    .with_compaction(Arc::new(compaction::CompactionOrchestrator::new(threshold)));
    (Arc::new(orch), output)
}

/// Seed `n` filler user messages so the token estimate clears a small threshold
/// (matches `proactive_compaction_test.rs`).
async fn seed_history(orch: &ConversationOrchestrator, n: usize) {
    let session = orch.session();
    let mut s = session.lock().await;
    for i in 0..n {
        if i % 2 == 0 {
            s.history.push(ConversationMessage::user(
                MessageId::new(),
                format!("turn-{i} body padded with filler text to push token count up beyond autocompact threshold"),
            ));
        } else {
            s.history.push(ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: format!("reply-{i} with enough detail for compaction"),
                }],
                stop_reason: Some("end_turn".into()),
            });
        }
    }
}

#[tokio::test]
async fn pre_and_post_compact_hooks_fire_on_proactive_autocompact() {
    let probe = Arc::new(Probe::default());
    let hooks = hook_executor_with(vec![
        (
            Arc::new(RecordPreCompact {
                probe: probe.clone(),
                fail: false,
                block: false,
            }) as Arc<dyn BuiltinHookHandler>,
            builtin_hook("record-pre-compact", HookEventType::PreCompact),
        ),
        (
            Arc::new(RecordPostCompact {
                probe: probe.clone(),
            }) as Arc<dyn BuiltinHookHandler>,
            builtin_hook("record-post-compact", HookEventType::PostCompact),
        ),
    ])
    .await;

    let (orch, output) = make_orch(hooks, 100);
    seed_history(&orch, 60).await;

    orch.run_turn("hello").await.expect("turn ok");

    // Sanity: the proactive trigger actually compacted (boundary marker +
    // CompactionCompleted), so the hooks had something to fire around.
    let events = output.snapshot().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::CompactionCompleted { .. })),
        "compaction must have run for this test to be meaningful"
    );

    // PreCompact fired with trigger=auto, BEFORE the summary (TS firing order).
    assert!(
        probe.pre_fired.load(Ordering::SeqCst),
        "PreCompact hook must fire when the proactive trigger compacts"
    );
    assert_eq!(
        probe.pre_trigger.lock().unwrap().as_deref(),
        Some("auto"),
        "the proactive/automatic compaction path must carry trigger=auto"
    );

    // PostCompact fired AFTER, carrying the produced summary.
    assert!(
        probe.post_fired.load(Ordering::SeqCst),
        "PostCompact hook must fire after compaction is applied"
    );
    assert!(
        probe.post_summary.lock().unwrap().is_some(),
        "PostCompact must carry the compaction summary payload"
    );
    // P2-04: the proactive/automatic path carries trigger=auto, threaded onto
    // the PostCompact event (so a matcher of "auto" would filter to it).
    assert_eq!(
        probe.post_trigger.lock().unwrap().as_deref(),
        Some("auto"),
        "PostCompact from the proactive autocompact path must carry trigger=auto"
    );
}

#[tokio::test]
async fn blocking_pre_compact_hook_skips_compaction_but_not_the_turn() {
    // Claude's `blockedBy` result stops the compaction attempt, but proactive
    // compaction is best-effort so the original model turn still proceeds.
    let probe = Arc::new(Probe::default());
    let hooks = hook_executor_with(vec![(
        Arc::new(RecordPreCompact {
            probe: probe.clone(),
            fail: true,
            block: true,
        }) as Arc<dyn BuiltinHookHandler>,
        builtin_hook("record-pre-compact", HookEventType::PreCompact),
    )])
    .await;

    let (orch, output) = make_orch(hooks, 100);
    seed_history(&orch, 60).await;

    // Turn must still succeed despite the blocked PreCompact pass.
    orch.run_turn("hello").await.expect("turn must not fail");

    assert_eq!(
        output.compaction_phase_snapshot().await,
        ["preparing", "error"]
    );

    // The hook fired ...
    assert!(
        probe.pre_fired.load(Ordering::SeqCst),
        "the (failing) PreCompact hook must still fire"
    );
    // ... but the compact transition itself must not happen.
    let events = output.snapshot().await;
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, OutputEvent::CompactionCompleted { .. })),
        "a blocking PreCompact hook must abort compaction"
    );
}

#[tokio::test]
async fn manual_compact_runs_reload_session_start_then_post_compact() {
    let probe = Arc::new(Probe::default());
    let setup = Arc::new(RecordCompactSetup {
        probe: probe.clone(),
    });
    let hooks = hook_executor_with(vec![
        (
            Arc::new(RecordPreCompact {
                probe: probe.clone(),
                fail: false,
                block: false,
            }) as Arc<dyn BuiltinHookHandler>,
            builtin_hook("record-pre-compact", HookEventType::PreCompact),
        ),
        (
            setup.clone() as Arc<dyn BuiltinHookHandler>,
            builtin_hook("record-compact-setup", HookEventType::InstructionsLoaded),
        ),
        (
            setup as Arc<dyn BuiltinHookHandler>,
            builtin_hook("record-compact-setup", HookEventType::SessionStart),
        ),
        (
            Arc::new(RecordPostCompact {
                probe: probe.clone(),
            }) as Arc<dyn BuiltinHookHandler>,
            builtin_hook("record-post-compact", HookEventType::PostCompact),
        ),
    ])
    .await;

    let cwd = std::env::temp_dir();
    let memory = MemoryFile {
        path: cwd.join("LINGXI.md"),
        body: "compact test instructions".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "compact test instructions".into(),
        content_differs_from_disk: false,
    };
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(CompactSummaryClient), "test-model".into()),
    );
    let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
        u64::MAX,
    ));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![memory])),
        cwd,
    )
    .with_cache_safe_slot(slot)
    .with_compaction(compactor);
    seed_history(&orch, 6).await;

    lingxi_core::host::OrchestratorHandle::force_compact(&orch)
        .await
        .expect("manual compaction");

    assert_eq!(
        probe.post_summary.lock().unwrap().as_deref(),
        Some("<summary>hook lifecycle summary</summary>"),
        "PostCompact receives the original summary, before continuation formatting"
    );

    assert_eq!(
        *probe.lifecycle_order.lock().unwrap(),
        vec![
            "pre".to_string(),
            format!("instructions:{:?}", InstructionsLoadReason::Compact),
            "session_start:compact".to_string(),
            "post".to_string(),
        ]
    );
    let history = orch.session().lock().await.history.clone();
    assert!(history.iter().any(|message| message
        .text_content()
        .contains("SessionStart hook additional context: post-compact hook context")));
}

struct InstructionHook {
    id: &'static str,
    stdout: &'static str,
}

#[async_trait]
impl BuiltinHookHandler for InstructionHook {
    fn id(&self) -> &str {
        self.id
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: self.stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
            response: None,
        }
    }
}

#[derive(Default)]
struct CaptureSummaryPrompt(Mutex<Option<String>>);

#[async_trait]
impl sidequery::SideQueryClient for CaptureSummaryPrompt {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        *self.0.lock().unwrap() = request
            .messages
            .last()
            .map(ConversationMessage::text_content);
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>ok</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

#[tokio::test]
async fn pre_compact_stdout_is_js_trimmed_and_joined_with_blank_lines() {
    let hooks = hook_executor_with(vec![
        (
            Arc::new(InstructionHook {
                id: "first",
                stdout: "\u{feff} first \n",
            }),
            builtin_hook("first", HookEventType::PreCompact),
        ),
        (
            Arc::new(InstructionHook {
                id: "second",
                stdout: "\tsecond\u{feff}",
            }),
            builtin_hook("second", HookEventType::PreCompact),
        ),
    ])
    .await;
    let client = Arc::new(CaptureSummaryPrompt::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new().with_side_query_client(client.clone(), "test".into()),
    );
    let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
        u64::MAX,
    ));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_cache_safe_slot(slot)
    .with_compaction(compactor);
    seed_history(&orch, 6).await;
    orch.force_compact_with_instructions_and_cancel(
        Some("focus"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        client.0.lock().unwrap().as_deref(),
        Some(compaction::prompt::get_compact_prompt(Some("focus\n\nfirst\n\nsecond")).as_str())
    );
}
