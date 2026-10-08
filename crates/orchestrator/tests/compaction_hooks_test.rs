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
            cache_control: None, citations: None,
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

async fn mod_host_registry(
    source: &str,
) -> (tempfile::TempDir, Arc<tokio::sync::RwLock<HookRegistry>>) {
    let dir = tempfile::tempdir().expect("mod directory");
    let module = dir.path().join("session-compact.js");
    std::fs::write(&module, source).expect("mod source");
    let host = hooks::mods::ModHost::start(None).await.expect("mod host");
    host.load(
        "session-compact-test",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load session.compact mod");
    let mut registry = HookRegistry::new();
    registry.set_mod_host(host);
    (dir, Arc::new(RwLock::new(registry)))
}

async fn make_mod_orch(
    registry: Arc<tokio::sync::RwLock<HookRegistry>>,
    compactor: Arc<compaction::CompactionOrchestrator>,
    slot: Arc<sidequery::CacheSafeParamsSlot>,
    analytics_bus: Option<Arc<telemetry::AnalyticsBus>>,
) -> (Arc<ConversationOrchestrator>, Arc<MockOutputStream>) {
    let hooks = hook_executor_with(Vec::new()).await;
    let response = || {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".to_string(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        )
    };
    let api = Arc::new(MockApiClient::new(vec![response(), response()]));
    let output = Arc::new(MockOutputStream::new());
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_cache_safe_slot(slot)
    .with_compaction(compactor)
    .with_hook_registry(registry);
    if let Some(analytics_bus) = analytics_bus {
        orch = orch.with_analytics_bus(analytics_bus);
    }
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
            s.history.push(ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: format!("reply-{i} with enough detail for compaction"), citations: None,
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
        parent: None,
        source_content: None,
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

#[derive(Default)]
struct CompactRequestProbe {
    calls: AtomicU32,
    requests: Mutex<Vec<Vec<String>>>,
}

struct CaptureCompactRequest(Arc<CompactRequestProbe>);

#[async_trait]
impl sidequery::SideQueryClient for CaptureCompactRequest {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(ConversationMessage::text_content)
                .collect(),
        );
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>core summary</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn compactor_with_probe(
    probe: Arc<CompactRequestProbe>,
    slot: Arc<sidequery::CacheSafeParamsSlot>,
    threshold: u64,
) -> Arc<compaction::CompactionOrchestrator> {
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(CaptureCompactRequest(probe)), "test-model".into()),
    );
    Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, slot),
        threshold,
    ))
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

#[tokio::test]
async fn manual_session_compact_mod_rewrites_next_input_and_applies_replacement_rows() {
    let (_mod_dir, registry) = mod_host_registry(
        r#"export function register(on) {
          on('session.compact', async ($, e, next) => {
            await $.ui.log(`manual-input:${e.trigger}:${e.instructions ?? ''}`);
            const core = await next({
              ...e,
              instructions: 'MOD_FOCUS_SENTINEL',
              messages: e.messages.map((row, index) => index === 0
                ? { ...row, text: 'MOD_INPUT_SENTINEL' }
                : row),
            });
            await $.ui.log(`manual-core:${core.messages[0].handle}`);
            return {
              ...core,
              messages: core.messages.map((row, index) => index === 0
                ? { ...row, text: 'MOD_REPLACEMENT_SENTINEL' }
                : row),
            };
          });
        }"#,
    )
    .await;
    let probe = Arc::new(CompactRequestProbe::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let compactor = compactor_with_probe(probe.clone(), slot.clone(), u64::MAX);
    let (orch, output) = make_mod_orch(registry, compactor, slot, None).await;
    seed_history(&orch, 6).await;

    orch.force_compact_with_instructions_and_cancel(
        Some("original manual focus"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("manual compact should apply the Mod replacement");

    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    let requests = probe.requests.lock().unwrap();
    let request_text = requests[0].join("\n");
    assert!(request_text.contains("MOD_INPUT_SENTINEL"));
    assert!(request_text.contains("MOD_FOCUS_SENTINEL"));
    drop(requests);

    let events = output.snapshot().await;
    let core_handle = events
        .iter()
        .find_map(|event| match event {
            OutputEvent::ModLog { text, .. } => {
                text.strip_prefix("manual-core:").map(str::to_string)
            }
            _ => None,
        })
        .expect("the Mod must observe the actual result returned by next(e)");
    let history = orch.session().lock().await.history.clone();
    let replacement = history
        .iter()
        .find(|message| message.text_content() == "MOD_REPLACEMENT_SENTINEL")
        .expect("the post-next replacement row must become session history");
    assert_ne!(
        replacement.id().as_uuid().to_string(),
        core_handle,
        "native replacement rows receive fresh UUIDs; the unchanged core result keeps its UUID"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        OutputEvent::ModLog { text, .. } if text.starts_with("manual-input:manual:")
    )));
}

#[tokio::test]
async fn proactive_auto_session_compact_next_rewrites_and_replaces_core_rows() {
    let (_mod_dir, registry) = mod_host_registry(
        r#"export function register(on) {
          on('session.compact', async ($, e, next) => {
            await $.ui.log(`auto-input:${e.trigger}`);
            const core = await next({
              ...e,
              instructions: 'AUTO_FOCUS_SENTINEL',
              messages: e.messages.map((row, index) => index === 0
                ? { ...row, text: 'AUTO_INPUT_SENTINEL' }
                : row),
            });
            await $.ui.log(`auto-core:${core.messages[0].handle}`);
            return {
              ...core,
              messages: core.messages.map((row, index) => index === 0
                ? { ...row, text: 'AUTO_REPLACEMENT_SENTINEL' }
                : row),
            };
          });
        }"#,
    )
    .await;
    let probe = Arc::new(CompactRequestProbe::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let compactor = compactor_with_probe(probe.clone(), slot.clone(), 100);
    let (orch, output) = make_mod_orch(registry, compactor, slot, None).await;
    seed_history(&orch, 60).await;

    orch.run_turn("hello")
        .await
        .expect("the turn should proceed after proactive compaction");

    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    let request_text = probe.requests.lock().unwrap()[0].join("\n");
    assert!(request_text.contains("AUTO_INPUT_SENTINEL"));
    assert!(request_text.contains("AUTO_FOCUS_SENTINEL"));
    let events = output.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        event,
        OutputEvent::ModLog { text, .. } if text == "auto-input:auto"
    )));
    let core_handle = events
        .iter()
        .find_map(|event| match event {
            OutputEvent::ModLog { text, .. } => text.strip_prefix("auto-core:").map(str::to_string),
            _ => None,
        })
        .expect("the Mod must see the real auto compact result");
    let history = orch.session().lock().await.history.clone();
    let replacement = history
        .iter()
        .find(|message| message.text_content() == "AUTO_REPLACEMENT_SENTINEL")
        .expect("the auto replacement row must be applied");
    assert_ne!(replacement.id().as_uuid().to_string(), core_handle);
    assert!(events
        .iter()
        .any(|event| matches!(event, OutputEvent::CompactionCompleted { .. })));
}

#[tokio::test]
async fn proactive_auto_session_compact_skip_avoids_core_and_boundary() {
    let (_mod_dir, registry) = mod_host_registry(
        r#"export function register(on) {
          on('session.compact', async ($, e) => {
            await $.ui.log(`auto-skip:${e.trigger}`);
            return { skip: 'keep this conversation' };
          });
        }"#,
    )
    .await;
    let probe = Arc::new(CompactRequestProbe::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let compactor = compactor_with_probe(probe.clone(), slot.clone(), 100);
    let (orch, output) = make_mod_orch(registry, compactor, slot, None).await;
    seed_history(&orch, 60).await;

    orch.run_turn("hello")
        .await
        .expect("a proactive skip is best-effort");

    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    let events = output.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        event,
        OutputEvent::ModLog { text, .. } if text == "auto-skip:auto"
    )));
    assert!(events
        .iter()
        .all(|event| !matches!(event, OutputEvent::CompactionCompleted { .. })));
    assert!(!output
        .compaction_phase_snapshot()
        .await
        .iter()
        .any(|phase| phase == "summarizing"));
    let history = orch.session().lock().await.history.clone();
    assert!(history.iter().all(|message| {
        !matches!(message, ConversationMessage::System { subtype: Some(subtype), .. } if subtype == "compact_boundary")
    }));
}

#[tokio::test]
async fn proactive_auto_direct_session_compact_replacement_applies_without_running_core() {
    let (_mod_dir, registry) = mod_host_registry(
        r#"export function register(on) {
          on('session.compact', async ($, e) => {
            await $.ui.log(`auto-direct:${e.trigger}`);
            return {
              messages: [{ role: 'user', text: 'DIRECT_REPLACEMENT_SENTINEL', toolUses: [] }],
              tokensBefore: 123,
              tokensAfter: 7,
              usage: {
                input_tokens: 5,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
              },
            };
          });
        }"#,
    )
    .await;
    let probe = Arc::new(CompactRequestProbe::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let compactor = compactor_with_probe(probe.clone(), slot.clone(), 100);
    let sink = Arc::new(telemetry::InMemorySink::new());
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;
    let (orch, output) = make_mod_orch(registry, compactor, slot, Some(bus)).await;
    seed_history(&orch, 60).await;

    orch.run_turn("first")
        .await
        .expect("direct replacement should apply without a summary call");

    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    let events = output.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        event,
        OutputEvent::ModLog { text, .. } if text == "auto-direct:auto"
    )));
    assert!(events
        .iter()
        .any(|event| matches!(event, OutputEvent::CompactionCompleted { .. })));
    assert!(!output
        .compaction_phase_snapshot()
        .await
        .iter()
        .any(|phase| phase == "summarizing"));
    let history = orch.session().lock().await.history.clone();
    assert!(history
        .iter()
        .any(|message| message.text_content() == "DIRECT_REPLACEMENT_SENTINEL"));

    orch.run_turn("second")
        .await
        .expect("a second turn proves direct replacement did not set compact tracking");
    assert!(sink
        .events()
        .await
        .iter()
        .all(|event| event.name != telemetry::tengu::orchestrator::POST_AUTOCOMPACT_TURN));
}
