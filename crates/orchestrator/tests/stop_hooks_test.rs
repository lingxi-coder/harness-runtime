//!
//! Exercises the core Stop continuation contract (TS `query.ts:1262-1308`):
//! - a Stop hook `Block` (keep working) continues with the blocking message
//!   appended + `stop_hook_active=true`; consecutive blocks loop up to
//!   `LINGXI_STOP_HOOK_BLOCK_CAP` (default 8), then the cap surfaces an
//!   override warning and ends the turn (no infinite loop);
//! - `preventContinuation` (`continue:false`) terminates as `StopHookPrevented`;
//! - a `UserPromptSubmit` `Block` aborts the turn BEFORE any API call.
use llm_runtime::ContentBlock as LlmContentBlock;

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
use hooks::{HookExecutorImpl, HookPromptRunner, PromptHookError, PromptHookRequest};
use lingxi_core::host::{
    HttpError, HttpTransport, OrchestratorHandle, RuntimeError, RuntimeSpawner,
};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse, SessionId};
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, TurnOutcome,
};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

static GOAL_CAP_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

// ---- Builtin Stop / UserPromptSubmit handlers ----

/// Stop hook that ALWAYS blocks (asks the agent to keep working) and carries a
/// system message.
struct StopBlockHandler;
#[async_trait]
impl BuiltinHookHandler for StopBlockHandler {
    fn id(&self) -> &str {
        "stop-block"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = matches!(event, HookEvent::Stop { .. }).then(|| HookResponse {
            decision: Some(HookDecision::Block),
            reason: Some("do X first".to_string()),
            // A transcript-only systemMessage that must NOT reach the model — the
            // continuation must come from `reason` (blockingError), never this.
            system_message: Some("[stop-hook] please continue".into()),
            ..Default::default()
        });
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response,
        }
    }
}

/// Stop hook that requests preventContinuation (`continue:false`).
struct StopPreventHandler;
#[async_trait]
impl BuiltinHookHandler for StopPreventHandler {
    fn id(&self) -> &str {
        "stop-prevent"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = matches!(event, HookEvent::Stop { .. }).then(|| HookResponse {
            prevent_continuation: true,
            ..Default::default()
        });
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response,
        }
    }
}

/// `UserPromptSubmit` hook that blocks the prompt.
struct PromptBlockHandler;
#[async_trait]
impl BuiltinHookHandler for PromptBlockHandler {
    fn id(&self) -> &str {
        "prompt-block"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = matches!(event, HookEvent::UserPromptSubmit { .. }).then(|| HookResponse {
            decision: Some(HookDecision::Block),
            reason: Some("prompt denied".to_string()),
            ..Default::default()
        });
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
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

async fn exec_with(
    handler: Arc<dyn BuiltinHookHandler>,
    hook: HookDefinition,
) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(hook);
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(handler);
    Arc::new(exec)
}

fn orch(api: Arc<MockApiClient>, hooks: Arc<HookExecutorImpl>) -> Arc<ConversationOrchestrator> {
    Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ))
}

fn orch_with_output(
    api: Arc<MockApiClient>,
    hooks: Arc<HookExecutorImpl>,
    output: Arc<MockOutputStream>,
) -> Arc<ConversationOrchestrator> {
    Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ))
}

/// The STREAMING twin of [`orch`].
///
/// `handle_stop_at_end` returns the same four `StopHookFlow` results to both
/// drivers, but each driver TRANSLATES them itself — streaming's match lives in
/// `conversation/drivers/mod.rs` and turns `Terminate` into
/// `Return(outcome)`, `TerminateMaxTurns` into `Err(MaxTurnsReached)`, and
/// `LoopAgain` into a recovery reset plus `Continue`. Until these tests every
/// row of that translation was unexercised: this file drove `run_turn` fifteen
/// times and `run_turn_streaming` zero.
fn streaming_orch(
    streams: Vec<Vec<llm_runtime::HistoryEvent>>,
    hooks: Arc<HookExecutorImpl>,
    config: OrchestratorConfig,
) -> Arc<ConversationOrchestrator> {
    ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        config,
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(orchestrator::test_support::MockStreamingApiClient::with_turns(streams)),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ))
}

/// One streamed `end_turn` round.
fn streamed_end_turn(text: &str) -> Vec<llm_runtime::HistoryEvent> {
    use orchestrator::test_support::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta,
    };
    vec![
        message_start("m", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, text),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]
}

fn end_turn(text: &str) -> llm_runtime::HistoryResponse {
    mock_message_response(
        vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )
}

fn end_turn_with_usage(text: &str, input: u64, output: u64) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "msg_goal".to_string(),
        model: "claude-opus-4-6".to_string(),
        content: vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
            citations: None,
        }],
        stop_reason: Some("end_turn".to_string()),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..llm_runtime::ExecutionUsage::default()
        },
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

struct ScriptedPromptRunner {
    seen: StdMutex<Vec<PromptHookRequest>>,
    scripted: StdMutex<VecDeque<Result<String, PromptHookError>>>,
    on_call: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[async_trait]
impl HookPromptRunner for ScriptedPromptRunner {
    async fn run(&self, req: PromptHookRequest) -> Result<String, PromptHookError> {
        self.seen.lock().unwrap().push(req);
        if let Some(on_call) = &self.on_call {
            on_call();
        }
        self.scripted
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(PromptHookError::Query("no scripted prompt response".into())))
    }
}

async fn exec_with_prompt_runner(runner: Arc<dyn HookPromptRunner>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    Arc::new(
        HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
            .with_prompt_runner(runner),
    )
}

#[tokio::test]
async fn stop_block_continues_until_cap_then_overrides() {
    let _env = GOAL_CAP_ENV_LOCK.lock().unwrap();
    let prior_cap = std::env::var("LINGXI_STOP_HOOK_BLOCK_CAP").ok();
    std::env::remove_var("LINGXI_STOP_HOOK_BLOCK_CAP");
    // A perpetually-blocking Stop hook drives consecutive continuations up to
    // the default LINGXI_STOP_HOOK_BLOCK_CAP of 8 (binary v2.1.191:
    // `bo=Number.isNaN(jr)?8:jr; if(bo>0&&ar>bo) …yield warning…{reason:"completed"}`),
    // NOT the old boolean guard that stopped after ONE continuation. Blocks 1..8
    // loop (counts 1..8 ≤ 8); the 9th block has next-count 9 > 8 ⇒ the cap fires,
    // surfaces the override warning, and ends the turn. So 9 API calls.
    let api = Arc::new(MockApiClient::new(vec![
        end_turn("1"),
        end_turn("2"),
        end_turn("3"),
        end_turn("4"),
        end_turn("5"),
        end_turn("6"),
        end_turn("7"),
        end_turn("8"),
        end_turn("9"),
    ]));
    let hooks = exec_with(
        Arc::new(StopBlockHandler),
        builtin_hook("stop-block", HookEventType::Stop),
    )
    .await;
    // Build with a CAPTURED output stream so we can assert the override warning.
    let output = Arc::new(MockOutputStream::new());
    let o = Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "the cap must end the turn, not loop forever: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        9,
        "a perpetually-blocking Stop hook drives 8 continuations (cap=8), ending on the 9th call"
    );
    // The byte-exact override warning is surfaced once the cap is exceeded
    // (em-dash U+2014; count 9 == the would-be next consecutive block).
    let texts = output.text_events().await;
    assert!(
        texts.iter().any(|t| t == "A hook blocked the turn from ending 9 consecutive times — overriding and ending turn. For Stop/SubagentStop hooks, check stop_hook_active in the input and return success while it's true. Set LINGXI_STOP_HOOK_BLOCK_CAP to raise this limit."),
        "the byte-exact override warning must be surfaced when the cap is exceeded: {texts:?}"
    );
    // The blocking REASON (blockingError) is appended as a meta user message,
    // wrapped exactly as claude-code's `getStopHookMessage`
    // (`utils/hooks.ts:1895`): "Stop hook feedback:\n<reason>". The transcript-
    // only systemMessage must NOT appear (it must never reach the model).
    let history = o.session().lock().await.history.clone();
    let appended = history
        .iter()
        .find(|m| m.text_content() == "Stop hook feedback:\ndo X first")
        .expect("the Stop hook feedback (from `reason`) must be appended to history verbatim");
    assert!(
        appended.is_meta(),
        "the appended Stop-hook-feedback user message must be marked meta (TS isMeta:true)"
    );
    assert!(
        !history
            .iter()
            .any(|m| m.text_content().contains("[stop-hook] please continue")),
        "the transcript-only systemMessage must NOT be appended (it must not reach the model)"
    );
    if let Some(prior_cap) = prior_cap {
        std::env::set_var("LINGXI_STOP_HOOK_BLOCK_CAP", prior_cap);
    }
}

#[tokio::test]
async fn stop_hook_block_cap_accepts_claude_code_env_alias() {
    let _env = GOAL_CAP_ENV_LOCK.lock().unwrap();
    let prior_lx = std::env::var("LINGXI_STOP_HOOK_BLOCK_CAP").ok();
    let prior_cc = std::env::var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP").ok();
    std::env::remove_var("LINGXI_STOP_HOOK_BLOCK_CAP");
    std::env::set_var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", "1");

    let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
    let hooks = exec_with(
        Arc::new(StopBlockHandler),
        builtin_hook("stop-block", HookEventType::Stop),
    )
    .await;
    let output = Arc::new(MockOutputStream::new());
    let o = Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP=1 must cap after the first continuation"
    );
    let texts = output.text_events().await;
    assert!(
        texts
            .iter()
            .any(|t| t.contains("overriding and ending turn")),
        "cap warning must fire when only the CLAUDE_CODE_ alias is set: {texts:?}"
    );

    std::env::remove_var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP");
    if let Some(v) = prior_lx {
        std::env::set_var("LINGXI_STOP_HOOK_BLOCK_CAP", v);
    }
    if let Some(v) = prior_cc {
        std::env::set_var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", v);
    }
}

#[tokio::test]
async fn stop_block_coinciding_with_max_turns_ends_without_feedback() {
    // Binary blocking-branch order: max-turns is checked BEFORE the block cap
    // (`let dt=ie+1,nn=te+1; if(c&&dt>c) return G("tengu_stop_hook_block_count",
    // {count:nn,hit_max_turns:!0,hit_cap:!1}),…,{reason:"max_turns",turnCount:dt}`).
    // So when a blocking Stop hook fires on the turn that hits `max_turns`, the
    // turn ends on the MAX-TURNS terminal and — unlike the `LoopAgain` path — does
    // NOT append the stop-hook feedback message (the binary returns before the
    // append). With `max_turns = 1`: turn 1 runs, the hook blocks, `turn_count(1)
    // >= max_turns(1)` ⇒ end as MaxTurnsReached, exactly ONE API call, and the
    // "Stop hook feedback:…" message must be absent from history.
    let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
    let hooks = exec_with(
        Arc::new(StopBlockHandler),
        builtin_hook("stop-block", HookEventType::Stop),
    )
    .await;
    let mut cfg = OrchestratorConfig::default();
    cfg.max_turns = 1;
    let o = Arc::new(ConversationOrchestrator::new(
        cfg,
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    let result = o.run_turn("hi").await;
    assert!(
        matches!(
            result,
            Err(orchestrator::OrchestratorError::MaxTurnsReached { max_turns: 1 })
        ),
        "a blocking Stop hook coinciding with max_turns must end on the max-turns \
         terminal, not the block cap: {result:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        1,
        "exactly one API call — the blocking branch ends immediately at max_turns"
    );
    let history = o.session().lock().await.history.clone();
    assert!(
        !history
            .iter()
            .any(|m| m.text_content() == "Stop hook feedback:\ndo X first"),
        "the stop-hook feedback must NOT be appended when the block coincides with \
         max_turns (the binary returns before appending): {:?}",
        history.iter().map(|m| m.text_content()).collect::<Vec<_>>()
    );
}

/// CANCELABLE: the same blocking-Stop-hook-at-`max_turns` fixture ends on a
/// DIFFERENT terminal than `run_turn`'s, and this asserts the difference rather
/// than either side alone.
///
/// `handle_stop_at_end` hands both batched entries the same
/// `StopHookFlow::TerminateMaxTurns`, and each names it itself: `run_turn`
/// raises `Err(MaxTurnsReached)`, `run_turn_with_cancel` returns
/// `Ok(TurnOutcome::MaxTurns)` — mirroring its own top-of-loop guard, which is
/// the whole reason §3.2 keeps the two mappings apart. Both call sites now sit
/// four lines apart on the same shared round, which is exactly the shape a later
/// "these are the same, merge them" tidy-up reaches for. Before this test that
/// tidy-up was silent: the twin and the streaming row were covered, this row
/// was driven zero times, and rewriting the cancelable arm to raise `Err` like
/// its neighbour left the whole suite green.
#[tokio::test]
async fn the_cancelable_entry_reports_a_stop_block_at_max_turns_as_an_outcome() {
    async fn fixture(max_turns: u32) -> (Arc<ConversationOrchestrator>, Arc<MockApiClient>) {
        let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
        let hooks = exec_with(
            Arc::new(StopBlockHandler),
            builtin_hook("stop-block", HookEventType::Stop),
        )
        .await;
        let mut cfg = OrchestratorConfig::default();
        cfg.max_turns = max_turns;
        let orch = Arc::new(ConversationOrchestrator::new(
            cfg,
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            hooks,
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        ));
        (orch, api)
    }

    // The cancelable entry, with a token that never fires: the turn reaches the
    // Stop hook normally, and the hook blocks on the turn that hits the cap.
    let (cancelable, cancelable_api) = fixture(1).await;
    let cancelable_result = cancelable
        .run_turn_with_cancel("hi", CancellationToken::new())
        .await;
    assert_eq!(
        cancelable_api.captured_msgs().await.len(),
        1,
        "exactly one API call — the blocking branch must have ended at max_turns, \
         or the assertion below is about some other terminal"
    );
    assert!(
        matches!(cancelable_result, Ok(TurnOutcome::MaxTurns)),
        "run_turn_with_cancel must report the Stop-hook max-turns terminal as an \
         OUTCOME, matching its own top-of-loop guard: {cancelable_result:?}"
    );

    // The non-cancelable twin, same fixture, raises instead of returning.
    let (plain, plain_api) = fixture(1).await;
    let plain_result = plain.run_turn("hi").await;
    assert_eq!(plain_api.captured_msgs().await.len(), 1);
    assert!(
        matches!(
            plain_result,
            Err(orchestrator::OrchestratorError::MaxTurnsReached { max_turns: 1 })
        ),
        "run_turn must raise the same terminal: {plain_result:?}"
    );
}

#[tokio::test]
async fn stop_prevent_continuation_terminates() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("done")]));
    let hooks = exec_with(
        Arc::new(StopPreventHandler),
        builtin_hook("stop-prevent", HookEventType::Stop),
    )
    .await;
    let o = orch(api.clone(), hooks.clone());

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::StopHookPrevented { .. }),
        "preventContinuation must terminate as StopHookPrevented: {outcome:?}"
    );
    assert_eq!(api.captured_msgs().await.len(), 1, "exactly one API call");
}

#[tokio::test]
async fn user_prompt_submit_block_aborts_before_api() {
    // No API responses needed — a UserPromptSubmit block must abort before any
    // messages_create call.
    let api = Arc::new(MockApiClient::new(vec![]));
    let hooks = exec_with(
        Arc::new(PromptBlockHandler),
        builtin_hook("prompt-block", HookEventType::UserPromptSubmit),
    )
    .await;
    let o = orch(api.clone(), hooks.clone());

    let outcome = o.run_turn("blocked prompt").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::StopHookPrevented { .. }),
        "a UserPromptSubmit block aborts the turn: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        0,
        "no API call may happen when UserPromptSubmit blocks"
    );
}

#[tokio::test]
async fn stop_hook_pass_ends_normally() {
    // A Stop hook that returns no decision (the handler only acts on a
    // DIFFERENT event type) must NOT interfere: normal single-turn end.
    let api = Arc::new(MockApiClient::new(vec![end_turn("fin")]));
    let hooks = exec_with(
        // PromptBlockHandler only responds to UserPromptSubmit, so as a Stop
        // hook it returns no decision → Pass.
        Arc::new(PromptBlockHandler),
        builtin_hook("prompt-block", HookEventType::Stop),
    )
    .await;
    let o = orch(api.clone(), hooks.clone());

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(api.captured_msgs().await.len(), 1);
}

#[tokio::test]
async fn stop_goal_registers_named_prompt_hook_and_clears_when_met() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([
            Ok(r#"{"ok": false, "reason": "not yet"}"#.to_string()),
            Ok(r#"{"ok": true, "reason": "done"}"#.to_string()),
        ])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner.clone()).await;
    let o = orch(api.clone(), hooks.clone());

    o.set_active_goal("ship it").await;
    let current_session = o.current_session_id().await;
    assert!(
        hooks
            .get_session_named_hook(current_session, "__session_goal_stop")
            .await
            .is_some(),
        "setting /goal must register the named session Stop Prompt hook"
    );

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(api.captured_msgs().await.len(), 2);
    assert!(
        o.get_active_goal().await.is_none(),
        "a completed /goal condition must auto-clear itself"
    );
    assert!(
        hooks
            .get_session_named_hook(current_session, "__session_goal_stop")
            .await
            .is_none(),
        "completed /goal must remove the temporary Stop hook"
    );
    let history = o.session().lock().await.history.clone();
    assert!(
        history
            .iter()
            .any(|m| m.text_content() == "Stop hook feedback:\n[ship it]: not yet"),
        "an unmet goal should append blocking feedback from the /goal evaluator"
    );
    let seen = runner.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].prompt.starts_with("Based on the conversation transcript above, has the following stopping condition been satisfied? Answer based on transcript evidence only.\n\nCondition: ship it\n\nARGUMENTS: "));
    assert!(
        seen[0].prompt.contains(r#""hook_event_name":"Stop""#),
        "goal Stop hook must evaluate the Stop payload"
    );
}

#[tokio::test]
async fn goal_status_transcript_records_set_progress_and_one_terminal_achievement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
    let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([
            Ok(r#"{"ok": false, "reason": "not yet"}"#.to_string()),
            Ok(r#"{"ok": true, "reason": "done"}"#.to_string()),
        ])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner).await;
    let orchestrator = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            hooks,
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer),
    );

    orchestrator.set_active_goal("ship it").await;
    orchestrator.run_turn("hi").await.expect("turn ok");

    let statuses = goal_status_rows(&path);
    assert_eq!(
        statuses,
        [
            ("set".to_string(), 0),
            // 2.1.266 @4212300: a blocking evaluation is its own `not_met`
            // record, NOT a second `set` sentinel.
            ("not_met".to_string(), 1),
            ("achieved".to_string(), 2)
        ],
        "a successful evaluation must not emit a redundant terminal set record"
    );
    let terminal = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| {
            line["attachment"]["type"] == "goal_status" && line["attachment"]["met"] == true
        })
        .expect("achievement attachment");
    assert_eq!(terminal["attachment"]["reason"], "done");
}

#[tokio::test]
async fn goal_status_transcript_records_impossible_as_failed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
    let api = Arc::new(MockApiClient::new(vec![end_turn("1")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([Ok(
            r#"{"ok": false, "impossible": true, "reason": "required service is unavailable"}"#
                .to_string(),
        )])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner).await;
    let orchestrator = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            hooks,
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer),
    );

    orchestrator.set_active_goal("ship it").await;
    orchestrator.run_turn("hi").await.expect("turn ok");
    assert!(orchestrator.get_active_goal().await.is_none());

    let statuses = goal_status_rows(path);
    assert_eq!(
        statuses,
        [("set".to_string(), 0), ("failed".to_string(), 1)],
        "an impossible condition must terminate as failed, never achieved"
    );
}

/// Read the `goal_status` records out of a transcript as `(variant, iterations)`.
///
/// 2.1.266 tells the five variants apart with `met`/`failed`/`sentinel` rather
/// than a `status` enum, and only the TERMINAL records carry `iterations` — the
/// set sentinel and the not-met record carry the count inside LingXi's
/// `goalState` resume extension instead.
fn goal_status_rows(path: impl AsRef<std::path::Path>) -> Vec<(String, u64)> {
    std::fs::read_to_string(path)
        .expect("goal transcript")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| {
            if line["type"] != "attachment" || line["attachment"]["type"] != "goal_status" {
                return None;
            }
            let a = &line["attachment"];
            let met = a["met"].as_bool()?;
            let failed = a["failed"].as_bool().unwrap_or(false);
            let sentinel = a["sentinel"].as_bool().unwrap_or(false);
            let variant = match (sentinel, met, failed) {
                (true, false, _) => "set",
                (true, true, _) => "cleared",
                (false, true, _) => "achieved",
                (false, false, true) => "failed",
                (false, false, false) => "not_met",
            };
            let iterations = a["iterations"]
                .as_u64()
                .or_else(|| a["goalState"]["iterations"].as_u64())
                .unwrap_or_default();
            Some((variant.to_string(), iterations))
        })
        .collect()
}

#[tokio::test]
async fn stop_goal_shares_the_stop_hook_block_limit() {
    let _env = GOAL_CAP_ENV_LOCK.lock().unwrap();
    let prior_cap = std::env::var("LINGXI_STOP_HOOK_BLOCK_CAP").ok();
    std::env::set_var("LINGXI_STOP_HOOK_BLOCK_CAP", "1");

    let api = Arc::new(MockApiClient::new(vec![
        end_turn("1"),
        end_turn("2"),
        end_turn("3"),
    ]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([
            Ok(r#"{"ok": false, "reason": "still working"}"#.to_string()),
            Ok(r#"{"ok": false, "reason": "still working"}"#.to_string()),
            Ok(r#"{"ok": true, "reason": "done"}"#.to_string()),
        ])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner).await;
    let output = Arc::new(MockOutputStream::new());
    let o = orch_with_output(api.clone(), hooks, output.clone());
    o.set_active_goal("ship it").await;

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the second blocking evaluation must end this turn at cap=1"
    );
    assert_eq!(o.get_active_goal().await.unwrap().iterations, 2);
    assert!(
        output
            .text_events()
            .await
            .iter()
            .any(|text| text.contains("overriding and ending turn")),
        "goal continuations must share the generic stop-hook cap"
    );
    if let Some(prior_cap) = prior_cap {
        std::env::set_var("LINGXI_STOP_HOOK_BLOCK_CAP", prior_cap);
    } else {
        std::env::remove_var("LINGXI_STOP_HOOK_BLOCK_CAP");
    }
}

#[tokio::test]
async fn stop_goal_timeout_pauses_without_counting_a_verdict() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([
            Err(PromptHookError::Timeout(Duration::from_secs(30))),
            Ok(r#"{"ok": true, "reason": "done"}"#.to_string()),
        ])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner).await;
    let o = orch(api.clone(), hooks);
    o.set_active_goal("ship it").await;

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(api.captured_msgs().await.len(), 1);
    let goal = o
        .get_active_goal()
        .await
        .expect("timeout keeps the goal active");
    assert_eq!(goal.iterations, 0);
    assert_eq!(goal.last_reason, None);
    assert!(!o
        .session()
        .lock()
        .await
        .history
        .iter()
        .any(|m| m.text_content().starts_with("Stop hook feedback:")));
}

#[tokio::test]
async fn stop_goal_query_error_does_not_spin_or_count_a_verdict() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("1"), end_turn("2")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([
            Err(PromptHookError::Query("provider 500".into())),
            Ok(r#"{"ok": true, "reason": "done"}"#.to_string()),
        ])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner).await;
    let o = orch(api.clone(), hooks);
    o.set_active_goal("ship it").await;

    let outcome = o.run_turn("hi").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert_eq!(api.captured_msgs().await.len(), 1);
    let goal = o
        .get_active_goal()
        .await
        .expect("error keeps the goal active");
    assert_eq!(goal.iterations, 0);
    assert_eq!(goal.last_reason, None);
    assert!(!o
        .session()
        .lock()
        .await
        .history
        .iter()
        .any(|m| m.text_content().starts_with("Stop hook feedback:")));
}

#[tokio::test]
async fn stop_goal_explicit_cancel_escapes_before_second_goal_check() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("1")]));
    let cancel = CancellationToken::new();
    let cancel_for_runner = cancel.clone();
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([Ok(
            r#"{"ok": false, "reason": "not yet"}"#.to_string(),
        )])),
        on_call: Some(Arc::new(move || cancel_for_runner.cancel())),
    });
    let hooks = exec_with_prompt_runner(runner.clone()).await;
    let o = orch(api.clone(), hooks);
    o.set_active_goal("ship it").await;

    let outcome = o
        .run_turn_with_cancel("hi", cancel)
        .await
        .expect("cancel path returns a turn outcome");
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert_eq!(api.captured_msgs().await.len(), 1);
    assert_eq!(runner.seen.lock().unwrap().len(), 1);
    assert!(
        o.get_active_goal().await.is_some(),
        "explicit cancel must not auto-clear an unmet goal"
    );
}

#[tokio::test]
async fn stop_goal_hard_budget_escape_stops_before_second_goal_check() {
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_with_usage("1", 1_000, 500),
        end_turn_with_usage("2", 1_000, 500),
    ]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(Vec::new()),
        scripted: StdMutex::new(VecDeque::from([
            Ok(r#"{"ok": false, "reason": "not yet"}"#.to_string()),
            Ok(r#"{"ok": true, "reason": "done"}"#.to_string()),
        ])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner.clone()).await;
    let (tx, _rx) = mpsc::channel(8);
    let tracker = Arc::new(cost::CostTracker::new(
        SessionId::new(),
        Arc::new(cost::PricingCatalog::builtin_reference()),
        tx,
    ));
    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into();
    cfg.max_budget_nano_usd = Some(1_000_000);
    let o = Arc::new(
        ConversationOrchestrator::new(
            cfg,
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            hooks,
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_cost_tracker(tracker),
    );
    o.set_active_goal("ship it").await;

    let err = o
        .run_turn("hi")
        .await
        .expect_err("budget must stop the loop");
    assert!(
        matches!(
            err,
            orchestrator::OrchestratorError::MaxBudgetReached { .. }
        ),
        "hard budget remains an escape hatch, got {err:?}"
    );
    assert_eq!(api.captured_msgs().await.len(), 1);
    assert_eq!(
        runner.seen.lock().unwrap().len(),
        1,
        "the second goal check must not run once the hard budget is exceeded"
    );
    assert!(o.get_active_goal().await.is_some());
}

struct RunningGoalBackgroundTask;
#[async_trait]
impl orchestrator::StopHookSnapshotProvider for RunningGoalBackgroundTask {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        vec![hooks::HookBackgroundTask {
            id: "background-build".into(),
            r#type: "shell".into(),
            status: "running".into(),
            description: "build".into(),
            is_idle: false,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            name: None,
        }]
    }
    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        vec![]
    }
}

#[tokio::test]
async fn deferred_goal_never_calls_evaluator_or_leaks_blocking_feedback() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("waiting")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(vec![]),
        scripted: StdMutex::new(VecDeque::from([Ok(
            r#"{"ok":false,"reason":"still running"}"#.into(),
        )])),
        on_call: None,
    });
    let hooks = exec_with_prompt_runner(runner.clone()).await;
    let o = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        hooks.clone(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_stop_hook_snapshot(Arc::new(RunningGoalBackgroundTask));
    o.set_active_goal("ship it").await;
    o.run_turn("hi").await.unwrap();
    assert_eq!(api.captured_msgs().await.len(), 1);
    assert!(
        runner.seen.lock().unwrap().is_empty(),
        "deferred goal must not send an evaluator request"
    );
    assert_eq!(o.get_active_goal().await.unwrap().iterations, 0);
    assert!(hooks
        .get_session_named_hook(o.current_session_id().await, "__session_goal_stop")
        .await
        .is_some());
}

#[tokio::test]
async fn goal_reason_keeps_bracket_sequences_inside_the_condition_out_of_status() {
    let api = Arc::new(MockApiClient::new(vec![end_turn("one"), end_turn("two")]));
    let runner = Arc::new(ScriptedPromptRunner {
        seen: StdMutex::new(vec![]),
        scripted: StdMutex::new(VecDeque::from([
            Ok(r#"{"ok":false,"reason":"tests pending"}"#.into()),
            Err(PromptHookError::Timeout(Duration::from_secs(30))),
        ])),
        on_call: None,
    });
    let o = orch(api, exec_with_prompt_runner(runner).await);
    o.set_active_goal("verify ]: details").await;
    o.run_turn("hi").await.unwrap();
    let goal = o.get_active_goal().await.unwrap();
    assert_eq!(goal.last_reason.as_deref(), Some("tests pending"));
    assert_eq!(goal.iterations, 1);
}

// ── STREAMING twins of the four `StopHookFlow` results ───────────────────────
//
// §4 of the unified-driver plan marks all four rows "两者" — both drivers — and
// they are four DIFFERENT results, not shades of one. Every test above drives
// the batched entry; the streaming driver translates the same four itself, and
// none of that translation was covered. PR 4 shares the end handling, so each
// row needs a twin before the shared version can be called equivalent.

/// STREAMING: `preventContinuation` terminates as `StopHookPrevented`.
///
/// Streaming maps `Terminate(outcome)` straight through as `Return(outcome)`,
/// so the public outcome must be the same variant the batched entry returns —
/// NOT a plain `EndTurn`. `run_turn_with_cancel` is the one entry allowed to
/// flatten it (§4 records that loss and says to keep it).
#[tokio::test]
async fn streaming_stop_prevent_continuation_terminates() {
    let hooks = exec_with(
        Arc::new(StopPreventHandler),
        builtin_hook("stop-prevent", HookEventType::Stop),
    )
    .await;
    let o = streaming_orch(
        vec![streamed_end_turn("one")],
        hooks,
        OrchestratorConfig::default(),
    );

    let outcome = o.run_turn_streaming("go").await.expect("streaming turn");
    assert!(
        matches!(outcome, ConversationOutcome::StopHookPrevented { .. }),
        "streaming must surface StopHookPrevented, not a plain EndTurn; it returns \
         Terminate(outcome) unchanged. Got {outcome:?}"
    );
}

/// STREAMING: a blocking Stop hook loops the turn again.
///
/// `LoopAgain` becomes `Continue`, so a second round is opened. Two scripted
/// streams: if the hook's block were dropped the first round would end the turn
/// and the second stream would go unused.
#[tokio::test]
async fn streaming_stop_block_loops_the_turn_again() {
    let hooks = exec_with(
        Arc::new(StopBlockHandler),
        builtin_hook("stop-block", HookEventType::Stop),
    )
    .await;
    let mut cfg = OrchestratorConfig::default();
    // Bound the loop: the hook blocks forever, so let max_turns stop it rather
    // than the cap, which would take nine rounds.
    cfg.max_turns = 2;
    let o = streaming_orch(
        vec![streamed_end_turn("one"), streamed_end_turn("two")],
        hooks,
        cfg,
    );

    let result = o.run_turn_streaming("go").await;
    assert!(
        matches!(
            result,
            Err(orchestrator::OrchestratorError::MaxTurnsReached { .. })
        ),
        "a blocking Stop hook must keep the streaming loop going until a limit stops it; \
         got {result:?}"
    );
}

/// STREAMING: blocking on the turn that reaches `max_turns` ends on the
/// max-turns terminal, not on the block cap.
///
/// The batched twin (`stop_block_coinciding_with_max_turns_ends_without_feedback`)
/// pins that the max-turns check runs BEFORE the cap inside the shared helper.
/// This pins that streaming's translation of that result is
/// `Err(MaxTurnsReached)` rather than a silent end — a `Terminate` here would
/// look like a normal finish to every caller.
#[tokio::test]
async fn streaming_stop_block_at_max_turns_ends_on_the_max_turns_terminal() {
    let hooks = exec_with(
        Arc::new(StopBlockHandler),
        builtin_hook("stop-block", HookEventType::Stop),
    )
    .await;
    let mut cfg = OrchestratorConfig::default();
    cfg.max_turns = 1;
    let o = streaming_orch(
        vec![streamed_end_turn("one"), streamed_end_turn("two")],
        hooks,
        cfg,
    );

    let result = o.run_turn_streaming("go").await;
    match result {
        Err(orchestrator::OrchestratorError::MaxTurnsReached { max_turns }) => {
            assert_eq!(max_turns, 1, "the terminal reports the configured limit");
        }
        other => panic!(
            "expected MaxTurnsReached from TerminateMaxTurns; got {other:?}. Streaming turns \
             that result into an ERROR, not an outcome — collapsing it into Terminate would \
             read as a normal finish."
        ),
    }
}

/// STREAMING: a passing Stop hook falls through to the normal end.
///
/// `FallThrough` is the "no hook intervened" result, and it must stay
/// distinguishable from `LoopAgain`: exactly one round, ending normally.
#[tokio::test]
async fn streaming_stop_hook_pass_ends_normally() {
    let hooks = exec_with(
        // Same trick as the batched twin: PromptBlockHandler only answers
        // UserPromptSubmit, so registered on Stop it returns no decision ⇒ Pass.
        Arc::new(PromptBlockHandler),
        builtin_hook("prompt-block", HookEventType::Stop),
    )
    .await;
    let o = streaming_orch(
        vec![streamed_end_turn("one")],
        hooks,
        OrchestratorConfig::default(),
    );

    let outcome = o.run_turn_streaming("go").await.expect("streaming turn");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { turn_count: 1, .. }),
        "a passing Stop hook ends the turn after exactly one round; got {outcome:?}"
    );
}

// ── a tool-requested end is not a Stop-hook decision point ───────────────────
//
// A tool that sets `claude/endTurn` has already decided the turn is over, so
// both drivers fire the Stop hooks for telemetry and IGNORE the verdict
// (`fire_tool_result_end_stop_hooks`) rather than acting on it
// (`handle_stop_at_end`). That used to be guaranteed structurally — the
// tool-requested helper simply called the other function — and is now a
// `tool_requested_end` flag into one shared end-of-turn sequence, which is a
// flag that can be passed wrong. Passed wrong, a BLOCKING Stop hook resurrects
// a turn the tool ended, and nothing in the suite noticed.

/// A tool that ends the turn via `_meta: { "claude/endTurn": true }`.
struct EndsTurnTool;

#[async_trait]
impl tool_api::tool_trait::Tool for EndsTurnTool {
    fn name(&self) -> &str {
        "EndsTurn"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| serde_json::json!({ "type": "object" }));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), tool_api::tool_trait::ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(
        &self,
        _input: &serde_json::Value,
        _opts: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &tool_api::tool_trait::PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::tool_trait::ToolCallResult, tool_api::tool_trait::ToolError> {
        Ok(tool_api::tool_trait::ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({ "content": "done" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: Some(serde_json::json!({"_meta": {"claude/endTurn": true}})),
        })
    }
}

fn round_calling_ends_turn() -> Vec<llm_runtime::HistoryEvent> {
    use orchestrator::test_support::{
        content_block_stop, input_json_delta, message_delta_stop, message_start, message_stop,
    };
    vec![
        message_start("m1", "claude-opus-4-7"),
        orchestrator::test_support_stream::content_block_start_tool_use(
            0,
            lingxi_core::types::ToolUseId::new(),
            "EndsTurn",
        ),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ]
}

/// STREAMING: a blocking Stop hook must NOT reopen a turn a tool ended.
///
/// The control is its neighbour `streaming_stop_block_loops_the_turn_again`: on
/// a NATURAL end the very same handler keeps the loop going until `max_turns`
/// stops it. So a single round here is the tool-requested branch ignoring the
/// verdict, not a hook that failed to fire — and this test bounds `max_turns`
/// the same way, so a reopened turn surfaces as `MaxTurnsReached` rather than as
/// a quiet extra round.
#[tokio::test]
async fn a_blocking_stop_hook_does_not_reopen_a_tool_requested_end() {
    let api = Arc::new(
        orchestrator::test_support::MockStreamingApiClient::with_turns(vec![
            round_calling_ends_turn(),
            streamed_end_turn("must never be requested"),
        ]),
    );
    let mut registry = tool_api::registry::ToolRegistry::new();
    registry.register_builtin(Arc::new(EndsTurnTool));
    let hooks = exec_with(
        Arc::new(StopBlockHandler),
        builtin_hook("stop-block", HookEventType::Stop),
    )
    .await;
    let mut cfg = OrchestratorConfig::default();
    cfg.max_turns = 2;
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        cfg,
        Arc::new(MockApiClient::new(Vec::new())),
        api.clone(),
        Arc::new(registry),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    let result = orch.run_turn_streaming("go").await;
    assert!(
        result.is_ok(),
        "a tool asked the turn to end, so the Stop hooks fire for telemetry and \
         their verdict is IGNORED. Acting on the block reopens the turn and it \
         runs out of max_turns instead: {result:?}"
    );
    assert_eq!(
        api.captured_calls().await.len(),
        1,
        "exactly one round — the tool's decision stands"
    );
}
