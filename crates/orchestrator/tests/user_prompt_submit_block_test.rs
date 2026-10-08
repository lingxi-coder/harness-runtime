//! `UserPromptSubmit` hook block-message rendering (P2-04, parity 2.1.207).
//!
//! claude-code's prompt-hook block path (`dPs` / `Tc(...,"warning",void 0,!0)`)
//! renders a warning message when a `UserPromptSubmit` hook returns
//! `decision:"block"`:
//!
//! ```text
//! UserPromptSubmit operation blocked by hook:
//! <reason>
//!
//! Original prompt: <prompt>
//! ```
//!
//! and collapses to the bare `UserPromptSubmit operation blocked by hook:\n<reason>`
//! when the hook set `hookSpecificOutput.suppressOriginalPrompt`. Either way the
//! turn aborts before any API call (`shouldQuery:!1`). Previously LingXi dropped
//! the whole message (`fire_user_prompt_submit` returned a bare bool).

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse};
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use std::pin::Pin;
use std::sync::Arc;
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

/// A `UserPromptSubmit` hook that blocks with `reason` and optionally sets
/// `suppressOriginalPrompt`.
struct BlockingPromptHook {
    reason: Option<String>,
    suppress: bool,
}
#[async_trait]
impl BuiltinHookHandler for BlockingPromptHook {
    fn id(&self) -> &str {
        "block-prompt"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(HookResponse {
                decision: Some(HookDecision::Block),
                reason: self.reason.clone(),
                suppress_original_prompt: self.suppress,
                ..Default::default()
            }),
        }
    }
}

fn builtin_hook(handler_id: &str) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: handler_id.into(),
        events: vec![HookEventType::UserPromptSubmit],
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

async fn orch_with_blocking_hook(
    reason: Option<&str>,
    suppress: bool,
) -> (Arc<ConversationOrchestrator>, Arc<MockOutputStream>) {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry
        .write()
        .await
        .register(builtin_hook("block-prompt"));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(BlockingPromptHook {
        reason: reason.map(str::to_string),
        suppress,
    }));

    // The turn aborts before any API call, but wire a response anyway.
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "unreached".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    )]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        Arc::new(exec),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    (Arc::new(orch), output)
}

/// A blocking hook with a reason renders the warning WITH the original prompt.
#[tokio::test]
async fn block_renders_warning_with_original_prompt() {
    let (orch, output) = orch_with_blocking_hook(Some("policy says no"), false).await;

    let outcome = orch.run_turn("delete everything").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::StopHookPrevented { .. }),
        "a blocking UserPromptSubmit hook aborts the turn: {outcome:?}"
    );

    let texts = output.text_events().await;
    assert!(
        texts.iter().any(|t| t
            == "UserPromptSubmit operation blocked by hook:\npolicy says no\n\nOriginal prompt: delete everything"),
        "block warning must carry reason + original prompt, got {texts:?}"
    );
}

/// `suppressOriginalPrompt:true` collapses the warning to the bare reason.
#[tokio::test]
async fn block_with_suppress_omits_original_prompt() {
    let (orch, output) = orch_with_blocking_hook(Some("policy says no"), true).await;

    let outcome = orch.run_turn("delete everything").await.expect("turn ok");
    assert!(matches!(
        outcome,
        ConversationOutcome::StopHookPrevented { .. }
    ));

    let texts = output.text_events().await;
    assert!(
        texts
            .iter()
            .any(|t| t == "UserPromptSubmit operation blocked by hook:\npolicy says no"),
        "suppressOriginalPrompt must omit the `Original prompt:` tail, got {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("Original prompt:")),
        "no `Original prompt:` line when suppressed, got {texts:?}"
    );
}

/// A blocking hook that omits a reason falls back to `"Blocked by hook"`.
#[tokio::test]
async fn block_without_reason_uses_default() {
    let (orch, output) = orch_with_blocking_hook(None, false).await;

    orch.run_turn("hi").await.expect("turn ok");

    let texts = output.text_events().await;
    assert!(
        texts.iter().any(|t| {
            t
            == "UserPromptSubmit operation blocked by hook:\nBlocked by hook\n\nOriginal prompt: hi"
        }),
        "missing reason falls back to `Blocked by hook`, got {texts:?}"
    );
}
