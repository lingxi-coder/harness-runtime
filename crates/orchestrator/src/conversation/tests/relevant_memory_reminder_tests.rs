use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use memory::surfacing::SurfacedMemory;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tool_api::registry::ToolRegistry;

/// A runtime that actually RUNS the spawned future on the current tokio
/// runtime, so the prefetch's one-shot send fires (the shared
/// `noop_hook_executor` `UnusedRuntime` errors instead, which would leave the
/// channel unresolved). Cancel/sleep are no-ops — the prefetch task is
/// instantaneous.
struct InlineRuntime;
#[async_trait]
impl lingxi_core::host::RuntimeSpawner for InlineRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        tokio::spawn(task);
        Ok(lingxi_core::host::BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: 0,
        })
    }
    async fn sleep(&self, _d: std::time::Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

fn mem(path: &str, content: &str, age_days: u64) -> SurfacedMemory {
    SurfacedMemory {
        path: PathBuf::from(path),
        content: content.into(),
        age_days,
        mtime: SystemTime::UNIX_EPOCH,
    }
}

/// Build an orchestrator with NO prefetch wired (surfacing inert).
fn orch_bare() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
}

#[tokio::test]
async fn maybe_extract_session_memory_is_noop_without_handle() {
    // The inert default: no session-memory handle wired (and no cache slot)
    // ⇒ a strict no-op (no panic, nothing spawned), so the locked fixtures
    // stay byte-identical. The enabled path's extract+write is covered by
    // `memory::session_memory` tests; the composition-root wiring is gated
    // behind `LINGXI_SESSION_MEMORY` (default off).
    let orch = orch_bare();
    assert!(orch.compaction_runtime.session_memory.is_none());
    orch.maybe_extract_session_memory().await;
    assert!(orch.compaction_runtime.session_memory.is_none());
}

/// Build an orchestrator whose prefetch resolves to `seed`.
fn orch_with_seed(seed: Vec<SurfacedMemory>) -> ConversationOrchestrator {
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> = Arc::new(InlineRuntime);
    let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::with_fixed_result(
        runtime, seed,
    ));
    orch_bare().with_memory_prefetch(prefetch)
}

#[derive(Default)]
struct MemoryRouteClient(std::sync::Mutex<Vec<(String, Option<String>)>>);

#[async_trait]
impl sidequery::SideQueryClient for MemoryRouteClient {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.0
            .lock()
            .unwrap()
            .push((request.model, request.profile));
        Ok(sidequery::SideQueryResponse {
            text: None,
            structured: Some(
                lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                    &serde_json::json!({ "filenames": ["guide.md"] }),
                )
                .expect("generated memory result is representable"),
            ),
            tool_calls: Vec::new(),
            usage: Default::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

#[tokio::test]
async fn memory_prefetch_follows_live_model_and_profile_switches() {
    let dir = tempfile::tempdir().unwrap();
    let roots =
        memory::memdir::paths::memdir_roots_at(dir.path(), std::path::Path::new("/work"), false);
    std::fs::create_dir_all(&roots.user_memdir).unwrap();
    std::fs::write(roots.user_memdir.join("guide.md"), "USE RG").unwrap();
    let client = Arc::new(MemoryRouteClient::default());
    let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
        Arc::new(memory::selector::MemorySelector::new(client.clone())),
        Arc::new(InlineRuntime),
        roots,
    ));
    let orch = orch_bare().with_memory_prefetch(prefetch);
    let routes = [
        ("gpt-4o", "openai"),
        ("gemini-2.5-pro", "google"),
        ("shared-model", "gateway-a"),
        ("shared-model", "gateway-b"),
    ];

    for (model, profile) in routes {
        {
            let mut session = orch.session.lock().await;
            session.model = model.into();
            session.model_profile = Some(profile.into());
        }
        orch.start_memory_prefetch().await;
        let pending = orch
            .prompt_runtime
            .pending_memory_prefetch
            .lock()
            .await
            .take()
            .unwrap();
        assert_eq!(pending.take().await.len(), 1);
    }

    assert_eq!(
        *client.0.lock().unwrap(),
        routes
            .into_iter()
            .map(|(model, profile)| (model.to_string(), Some(profile.to_string())))
            .collect::<Vec<_>>()
    );
}

#[derive(Default)]
struct MemoryIntentClient {
    prompts: std::sync::Mutex<Vec<String>>,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl sidequery::SideQueryClient for MemoryIntentClient {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.prompts
            .lock()
            .unwrap()
            .push(request.messages[0].text_content());
        self.entered.notify_one();
        Ok(sidequery::SideQueryResponse {
            text: None,
            structured: Some(
                lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                    &serde_json::json!({ "filenames": [] }),
                )
                .unwrap(),
            ),
            tool_calls: Vec::new(),
            usage: Default::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

#[tokio::test]
async fn memory_selector_uses_latest_admitted_source_prompt_without_changing_its_units() {
    let ordinary = "current admitted request α"
        .encode_utf16()
        .collect::<Vec<_>>();
    let mut unpaired = "current admitted request "
        .encode_utf16()
        .collect::<Vec<_>>();
    unpaired.push(0xd800);
    unpaired.extend(" question".encode_utf16());
    for units in [ordinary, unpaired] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let roots = memory::memdir::paths::memdir_roots_at(root.path(), &cwd, false);
        std::fs::create_dir_all(&roots.user_memdir).unwrap();
        std::fs::write(roots.user_memdir.join("guide.md"), "candidate memory").unwrap();
        let client = Arc::new(MemoryIntentClient::default());
        let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
            Arc::new(memory::selector::MemorySelector::new(client.clone())),
            Arc::new(InlineRuntime),
            roots,
        ));
        let api = Arc::new(MockApiClient::new(vec![
            crate::test_support::mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    citations: None,
                    cache_control: None,
                }],
                Some("end_turn"),
            ),
        ]));
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd,
        )
        .with_memory_prefetch(prefetch);
        orch.session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(
                MessageId::new(),
                "obsolete prior request".into(),
            ));
        let display = String::from_utf16_lossy(&units);
        let source = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            display.clone(),
            units.clone(),
        )
        .unwrap();
        orch.run_turn(source).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), client.entered.notified())
            .await
            .expect("the real memory selector receives the admitted query");
        assert_eq!(client.prompts.lock().unwrap().len(), 1);
        assert!(client.prompts.lock().unwrap()[0]
            .starts_with(&format!("Query: {display}\n\nAvailable memory files:\n")));
        let requests = api.captured_msgs().await;
        let admitted = requests[0]
            .iter()
            .find(|message| !message.is_meta() && message.text_content() == display)
            .expect("the current user prompt reaches the real request");
        let ConversationMessage::User { content, .. } = admitted else {
            panic!("the admitted prompt is a user row");
        };
        assert!(matches!(
            &content[0],
            lingxi_core::types::ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                if utf16_code_units == &units
        ));
    }
}

#[tokio::test]
async fn no_prefetch_wired_yields_none() {
    let orch = orch_bare();
    // Without arming, and with no prefetch, the reminder is a strict no-op.
    orch.start_memory_prefetch().await;
    assert!(orch.relevant_memory_reminder_messages().await.is_empty());
    assert!(!orch.has_memory_prefetch());
}

#[tokio::test]
async fn empty_prefetch_result_yields_none() {
    let orch = orch_with_seed(vec![]);
    orch.start_memory_prefetch().await;
    assert!(orch.relevant_memory_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn not_armed_yields_none() {
    // A wired prefetch that was never armed this turn (slot empty) ⇒ None.
    let orch = orch_with_seed(vec![mem("/m/a.md", "A", 0)]);
    assert!(orch.relevant_memory_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn seeded_prefetch_renders_relevant_memories_block() {
    let orch = orch_with_seed(vec![mem("/m/a.md", "USE FD NOT FIND", 0)]);
    orch.start_memory_prefetch().await;
    let msg = orch
        .relevant_memory_reminder_messages()
        .await
        .pop()
        .expect("seeded prefetch must surface");
    let text = msg.text_content();
    assert!(msg.is_meta(), "relevant memory must be a meta user message");
    assert!(
        text.contains("Retrieved for possible relevance \u{2014} use only if it actually applies"),
        "idx-0 preamble missing: {text}"
    );
    assert!(
        text.contains("Memory: /m/a.md:\n\nUSE FD NOT FIND"),
        "got: {text}"
    );
}

#[tokio::test]
async fn surfaced_once_then_not_reinjected_across_turns() {
    let orch = orch_with_seed(vec![mem("/m/a.md", "A", 0)]);
    // Turn 0: surfaced.
    orch.start_memory_prefetch().await;
    assert!(
        !orch.relevant_memory_reminder_messages().await.is_empty(),
        "first surfacing must inject"
    );
    // Turn 1: same memory ⇒ already in surfaced_memory_paths ⇒ no re-inject.
    orch.start_memory_prefetch().await;
    assert!(
        orch.relevant_memory_reminder_messages().await.is_empty(),
        "an already-surfaced memory must not be re-injected"
    );
}

/// Seed the ONE shared read-state registry as a file tool's
/// `readFileState.set` would (path is all the dedup keys off).
fn seed_read_state(orch: &ConversationOrchestrator, path: PathBuf) {
    tool_api::read_file_state::set(
        &orch.prompt_runtime.read_state_map,
        path,
        tool_api::read_file_state::ReadFileEntry {
            content: String::new(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
}

fn seed_host_read_state(orch: &ConversationOrchestrator, path: PathBuf) {
    tool_api::read_file_state::set_with_model_context(
        &orch.prompt_runtime.read_state_map,
        path,
        tool_api::read_file_state::ReadFileEntry {
            content: String::new(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: false,
            seeded_from_context: false,
            is_partial_view: false,
        },
        false,
    );
}

#[tokio::test]
async fn shared_dedup_skips_memory_already_in_read_state_map() {
    // A memory whose path was already loaded as a nested/conditional (P3.2)
    // attachment / tool read (present in the shared read_state_map) must NOT
    // be double-injected via the surfacing channel.
    let orch = orch_with_seed(vec![mem("/m/a.md", "A", 0)]);
    seed_read_state(&orch, PathBuf::from("/m/a.md"));
    orch.start_memory_prefetch().await;
    assert!(
        orch.relevant_memory_reminder_messages().await.is_empty(),
        "a path already in read_state_map must not be surfaced"
    );
}

#[tokio::test]
async fn partial_dedup_surfaces_only_fresh_memories() {
    // Two memories; one already read. Only the fresh one surfaces, and it
    // carries the idx-0 preamble (it is the first RENDERED memory).
    let orch = orch_with_seed(vec![
        mem("/m/seen.md", "SEEN", 0),
        mem("/m/new.md", "NEW", 0),
    ]);
    seed_read_state(&orch, PathBuf::from("/m/seen.md"));
    orch.start_memory_prefetch().await;
    let messages = orch.relevant_memory_reminder_messages().await;
    let text = messages[0].text_content();
    assert!(text.contains("Memory: /m/new.md:\n\nNEW"), "got: {text}");
    assert!(
        !text.contains("/m/seen.md"),
        "already-read memory leaked: {text}"
    );
}

#[tokio::test]
async fn host_seeded_path_does_not_suppress_relevant_memory() {
    let orch = orch_with_seed(vec![mem("/m/seeded.md", "SEEDED", 0)]);
    seed_host_read_state(&orch, PathBuf::from("/m/seeded.md"));
    orch.start_memory_prefetch().await;
    let messages = orch.relevant_memory_reminder_messages().await;
    let text = messages[0].text_content();
    assert!(
        text.contains("Memory: /m/seeded.md:\n\nSEEDED"),
        "got: {text}"
    );
}

#[tokio::test]
async fn multiple_memories_keep_independent_meta_message_boundaries() {
    let orch = orch_with_seed(vec![mem("/m/a.md", "A", 0), mem("/m/b.md", "B", 0)]);
    orch.start_memory_prefetch().await;
    let messages = orch.relevant_memory_reminder_messages().await;
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(ConversationMessage::is_meta));
    assert!(messages[0].text_content().contains("Memory: /m/a.md:"));
    assert!(messages[1].text_content().contains("Memory: /m/b.md:"));
    assert!(!messages[1]
        .text_content()
        .contains("Retrieved for possible relevance"));
}
