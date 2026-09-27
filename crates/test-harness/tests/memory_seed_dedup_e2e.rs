//! END-TO-END proof that the seeded Read-dedup PRODUCE path is not inert.
//!
//! `orchestrator` does not depend on `tool-file`, so neither crate can host
//! this test alone. `test-harness` depends on `memory`, `tool-api`, `tool-file`
//! AND `orchestrator`, so it can wire the real production seam end to end:
//!
//!   1. one shared `ReadFileStateMap`,
//!   2. handed to the orchestrator via `.with_read_state_map(...)` (what the
//!      composition roots do) and to the file tools via
//!      `BuiltinToolContext::read_file_state`,
//!   3. the ALREADY-WIRED session-start seam `fire_instructions_loaded()`
//!      (called once by `apps/engine-desktop`),
//!   4. the REAL `FileReadTool` reading the seeded LINGXI.md.
//!
//! If step 3 stopped seeding, step 4 would return full content and this test
//! goes red — which is exactly what it is for.

use async_trait::async_trait;
use hooks::registry::HookRegistry;
use hooks::HookExecutorImpl;
use orchestrator::prompt::{MemoryHierarchyProvider, RealMemoryHierarchyProvider};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, MemoryFile, OrchestratorConfig};
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
async fn seeded_memory_file_dedups_on_a_real_read_tool_call() {
    let tmp = TempDir::new().unwrap();
    // Deliberately NOT canonicalized. On macOS `TempDir` hands back a `/var/...`
    // path whose real location is `/private/var/...`, which is exactly the
    // production shape: the orchestrator's cwd is whatever the user launched in,
    // symlinks and all, while `FileReadTool` looks the registry up by
    // `canonicalize_and_validate(..)`. Canonicalizing here made the test pass by
    // hiding that mismatch — the seed and the lookup only agreed because the
    // fixture had pre-resolved the path for them.
    let cwd = tmp.path().to_path_buf();
    let managed = tmp.path().join("empty-managed");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::create_dir_all(cwd.join(".lingxi/rules")).unwrap();

    // Rendered into the memory block -> seeded with `seededFromContext: true`.
    let plain = cwd.join("LINGXI.md");
    std::fs::write(&plain, "# repo rules\nbe careful\n").unwrap();
    // A `paths:`-gated rule: NOT in model context -> seeded with FALSE, so a
    // Read of it must return the FULL body.
    let cond = cwd.join(".lingxi/rules/cond.md");
    std::fs::write(&cond, "---\npaths: src/**\n---\nscoped body\n").unwrap();

    // Load the real hierarchy for this cwd (Managed pointed at an empty dir),
    // then keep only the fixture's own files so the developer's real
    // `~/.lingxi/LINGXI.md` cannot leak in.
    std::env::set_var(memory::lingxi_md::hierarchy::MANAGED_DIR_ENV, &managed);
    let files: Vec<MemoryFile> = RealMemoryHierarchyProvider
        .load(&cwd)
        .await
        .into_iter()
        .filter(|f| f.path.starts_with(&cwd))
        .collect();
    std::env::remove_var(memory::lingxi_md::hierarchy::MANAGED_DIR_ENV);
    assert!(
        files.iter().any(|f| f.path == plain) && files.iter().any(|f| f.path == cond),
        "fixture must load both files"
    );

    // ── ONE shared registry, exactly as a composition root wires it ─────────
    let shared = tool_api::read_file_state::new_read_file_state_map();

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(files)),
        cwd.clone(),
    )
    .with_read_state_map(Arc::clone(&shared));

    let mut tool_ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    tool_ctx.read_file_state = Arc::clone(&shared);
    let read_tool = tool_file::read::FileReadTool::new(tool_ctx);

    // Nothing seeded yet: a Read returns full content.
    let pre = read_tool
        .call(
            json!({ "file_path": plain.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        pre.data["file"]["content"], "# repo rules\nbe careful\n",
        "before seeding, Read must return the body"
    );
    // Drop that Read's own entry so the seeding guard (`!has(path)`) does not
    // legitimately skip it — this test is about the SEED, not the Read entry.
    //
    // By CANONICAL path: the Read tool keyed it under
    // `canonicalize_and_validate(..)`'s output, so removing the raw path is a
    // silent no-op. That in turn makes the seed skip (the entry is still there)
    // and the next Read dedup via the plain branch, which reports no `source` —
    // the exact failure this fixture used to hide by pre-canonicalizing `cwd`.
    shared
        .lock()
        .unwrap()
        .remove(&std::fs::canonicalize(&plain).unwrap());

    // ── THE PRODUCTION SEAM ────────────────────────────────────────────────
    orch.fire_instructions_loaded().await;

    // Rendered file -> seeded stub, byte-exact.
    let after = read_tool
        .call(
            json!({ "file_path": plain.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        after.data["type"], "file_unchanged",
        "the seeded memory file must dedup"
    );
    assert_eq!(after.data["source"], "seeded");
    assert_eq!(
        after.model_content.as_deref(),
        Some(tool_file::read::format_file_unchanged_seeded(&plain).as_str())
    );

    // Conditional rule -> `seededFromContext: false` -> FULL content.
    let scoped = read_tool
        .call(
            json!({ "file_path": cond.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_ne!(
        scoped.data["type"], "file_unchanged",
        "a paths:-gated rule is NOT in model context and must NOT dedup"
    );
    assert_eq!(
        scoped.data["file"]["content"], "---\npaths: src/**\n---\nscoped body\n",
        "the conditional rule must come back in full"
    );
}
