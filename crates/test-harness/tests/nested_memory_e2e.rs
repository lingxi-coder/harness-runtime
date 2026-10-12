//! END-TO-END proof that nested memory reaches the model and then dedups.
//!
//! `orchestrator` does not depend on `tool-file`, so neither crate can host
//! this alone. `test-harness` sees both, so it can wire the real seam:
//!
//!   1. one shared `ReadFileStateMap`, handed to the orchestrator via
//!      `.with_read_state_map(..)` and to the file tools via
//!      `BuiltinToolContext::read_file_state` — what a composition root does,
//!   2. a real model-dispatched Read of a nested source file, which queues the
//!      path through the production per-query trigger,
//!   3. the next turn preparation discovers and renders nested memory and
//!      seeds its surfaced file path,
//!   4. a real FileReadTool call on that surfaced path returns the seeded stub
//!      instead of the full file body.
//!
//! The temp dir is deliberately NOT canonicalized — see the note on `cwd`.

use async_trait::async_trait;
use hooks::registry::HookRegistry;
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, HttpRequest, HttpResponse, ToolUseId};
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tool_api::registry::ToolRegistry;
use tool_api::test_support::{fresh_ctx, fresh_tx};
use tool_api::tool_trait::Tool;

fn generated_input(value: serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).expect("generated tool input is valid")
}

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

#[tokio::test]
async fn nested_memory_surfaces_on_a_real_read_then_dedups_on_a_real_read() {
    let tmp = TempDir::new().unwrap();
    // Deliberately NOT canonicalized. On macOS `TempDir` hands back `/var/...`
    // whose real location is `/private/var/...`. Native nested discovery follows
    // the symlink when selecting directories, while Native `Kv(file_path)` and
    // FileReadTool's read-state keys preserve the model-visible lexical path.
    // This fixture checks both spellings explicitly: the reminder exposes the
    // canonical nested-memory path, and the re-read uses that same path to hit
    // the seed.
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
    let tool_use_id = ToolUseId::new();
    let api = Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: tool_use_id.to_string(),
                name: "Read".into(),
                input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(
                    &json!({"file_path": source.to_string_lossy()}),
                )
                .expect("generated file-read input"),
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "read complete".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let mut tool_ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    tool_ctx.read_file_state = Arc::clone(&shared);
    let mut tool_registry = ToolRegistry::new();
    let read_tool: Arc<dyn Tool> = Arc::new(tool_file::read::FileReadTool::new(tool_ctx));
    tool_registry.register_builtin(Arc::clone(&read_tool));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_registry),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::clone(&output) as Arc<dyn lingxi_core::host::OutputStream>,
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        cwd.clone(),
    )
    .with_read_state_map(Arc::clone(&shared))
    // Hermetic User/Managed roots: the real `~/.lingxi/rules` must not decide
    // whether this test passes.
    .with_nested_memory_roots(home, None);

    // Nothing touched yet → nothing to discover from.
    assert!(
        orch.nested_memory_reminder_messages(&[]).await.is_empty(),
        "no touched file, no nested memory"
    );

    // ── (2) A real model-dispatched Read marks the path on the same per-query
    // trigger queue that the next preparation consumes. This exercises the
    // production call context and frozen context-attachment input together.
    orch.run_turn("read the package source")
        .await
        .expect("read turn completes");
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 2, "Read result is followed by another query");
    let text = calls[1]
        .iter()
        .map(lingxi_core::types::ConversationMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n");
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

    // ── (4) Re-read the path actually surfaced to the model. Native's nested
    // discovery follows the `/var` cwd symlink when selecting nested
    // directories, so this fixture's model-facing path and seed key are the
    // canonical `/private/var/...` route. FileReadTool state keys are lexical
    // routes: passing the original `/var/...` spelling here would correctly
    // miss that seed and return the full text, even though both names reach the
    // same inode.
    let after = read_tool
        .call(
            generated_input(json!({ "file_path": canonical_mem.to_str().unwrap() })),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        after.data.string_at("/type"),
        Some("file_unchanged"),
        "the path shown to the model must hit the same nested-memory seed"
    );
    assert_eq!(after.data.string_at("/source"), Some("seeded"));

    // The oracle's `k$o` returns RECORDS; the reminder is one rendering of them
    // and the UI attachment line is the other. The port originally emitted only
    // the reminder, so the TUI's attachment cell had no producer. Assert the
    // record actually reaches the output stream, with the cwd-relative
    // `displayPath` the oracle computes.
    let attachments = output.attachment_snapshot().await;
    assert!(
        attachments.iter().any(|a| matches!(
            a,
            lingxi_core::host::AttachmentKind::NestedMemory { display_path }
                if display_path.ends_with("LINGXI.md") && !display_path.starts_with('/')
        )),
        "the surfaced nested memory must emit an attachment record, cwd-relative; got {attachments:?}"
    );
}
