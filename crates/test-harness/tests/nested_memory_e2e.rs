//! END-TO-END proof that nested memory reaches the model and then dedups.
//!
//! `orchestrator` does not depend on `tool-file`, so neither crate can host
//! this alone. `test-harness` sees both, so it can wire the real seam:
//!
//!   1. one shared `ReadFileStateMap`, handed to the orchestrator via
//!      `.with_read_state_map(..)` and to the file tools via
//!      `BuiltinToolContext::read_file_state` — what a composition root does,
//!   2. a REAL `FileReadTool` call on a nested source file, which is what marks
//!      it as touched,
//!   3. `nested_memory_reminder_message()`, the method both turn drivers call,
//!   4. a REAL `FileReadTool` call on the surfaced memory file, which must now
//!      return the seeded stub instead of the bytes.
//!
//! The temp dir is deliberately NOT canonicalized — see the note on `cwd`.

use async_trait::async_trait;
use hooks::registry::HookRegistry;
use hooks::HookExecutorImpl;
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use protocol::{HttpRequest, HttpResponse};
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tool_api::registry::ToolRegistry;
use tool_api::test_support::{fresh_ctx, fresh_tx};
use tool_api::tool_trait::Tool;

struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
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
    ) -> Result<platform_api::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(&self, _h: &platform_api::BackgroundTaskHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn nested_memory_surfaces_on_a_real_read_then_dedups_on_a_real_read() {
    let tmp = TempDir::new().unwrap();
    // Deliberately NOT canonicalized. On macOS `TempDir` hands back `/var/...`
    // whose real location is `/private/var/...`, which is the production shape:
    // the orchestrator's cwd is whatever the user launched in, symlinks and all,
    // while `FileReadTool` keys the registry by `canonicalize_and_validate(..)`.
    // Pre-canonicalizing here would let the seed and the lookup agree for the
    // wrong reason — it hid a real bug in the sibling seeding port.
    let cwd = tmp.path().to_path_buf();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let nested_mem = cwd.join("pkg").join(branding::MEMORY_FILE);
    let source = cwd.join("pkg").join("api").join("handler.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&nested_mem, "# pkg rules\nvalidate every input\n").unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();

    // ── ONE shared registry, exactly as a composition root wires it ─────────
    let shared = tool_api::read_file_state::new_read_file_state_map();

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::clone(&output) as Arc<dyn platform_api::OutputStream>,
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        cwd.clone(),
    )
    .with_read_state_map(Arc::clone(&shared))
    // Hermetic User/Managed roots: the real `~/.lingxi/rules` must not decide
    // whether this test passes.
    .with_nested_memory_roots(home, None);

    let mut tool_ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    tool_ctx.read_file_state = Arc::clone(&shared);
    let read_tool = tool_file::read::FileReadTool::new(tool_ctx);

    // Nothing touched yet → nothing to discover from.
    assert!(
        orch.nested_memory_reminder_message().await.is_none(),
        "no touched file, no nested memory"
    );

    // ── (2) a REAL Read of the nested source file marks it as touched ───────
    read_tool
        .call(
            json!({ "file_path": source.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();

    // ── (3) THE PRODUCTION SEAM — what both turn drivers call ───────────────
    let reminder = orch
        .nested_memory_reminder_message()
        .await
        .expect("the LINGXI.md governing pkg/ must surface");
    let text = reminder.text_content();
    assert!(
        text.contains("validate every input"),
        "the body must reach the model: {text}"
    );
    // Rendered under the CANONICAL path. Discovery walks from a canonicalized
    // cwd — it has to, because the touched-file keys it compares against come
    // from `canonicalize_and_validate` — so every path it derives is canonical
    // too. That is the form the seed is keyed under, which is what makes the
    // Read below dedup; asserting the raw `/var/...` form here would be
    // asserting a mismatch between what the model is shown and what the
    // registry holds.
    let canonical_mem = std::fs::canonicalize(&nested_mem).unwrap();
    assert!(
        text.contains(&format!("Contents of {}:", canonical_mem.display())),
        "rendered under the path the model can Read: {text}"
    );

    // ── (4) a REAL Read of the surfaced file must now dedup ─────────────────
    let after = read_tool
        .call(
            json!({ "file_path": nested_mem.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        after.data["type"], "file_unchanged",
        "the surfaced memory file must dedup — this is the whole point of \
         seeding it, and it only works if the seed used the CANONICAL path"
    );
    assert_eq!(after.data["source"], "seeded");

    // The oracle's `k$o` returns RECORDS; the reminder is one rendering of them
    // and the UI attachment line is the other. The port originally emitted only
    // the reminder, so the TUI's attachment cell had no producer. Assert the
    // record actually reaches the output stream, with the cwd-relative
    // `displayPath` the oracle computes.
    let attachments = output.attachment_snapshot().await;
    assert!(
        attachments.iter().any(|a| matches!(
            a,
            platform_api::AttachmentKind::NestedMemory { display_path }
                if display_path.ends_with("LINGXI.md") && !display_path.starts_with('/')
        )),
        "the surfaced nested memory must emit an attachment record, cwd-relative; got {attachments:?}"
    );
}
