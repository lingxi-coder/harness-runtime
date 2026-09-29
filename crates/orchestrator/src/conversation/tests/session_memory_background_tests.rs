use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use async_trait::async_trait;
use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
use std::sync::Arc;
use tokio::sync::Notify;
use tool_api::context::ToolUseOptions;

struct TokioRuntime;

#[async_trait]
impl lingxi_core::host::RuntimeSpawner for TokioRuntime {
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

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

struct BlockingSideQuery {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl SideQueryClient for BlockingSideQuery {
    async fn query(&self, _request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        self.started.notify_waiters();
        self.release.notified().await;
        Ok(SideQueryResponse {
            text: Some("durable notes from the old session".to_string()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".to_string()),
            retry_count: 0,
        })
    }
}

fn cache_safe_params() -> sidequery::CacheSafeParams {
    sidequery::CacheSafeParams {
        system_prompt: Arc::from("SYS"),
        tools: Vec::new(),
        effort: None,
        user_context: std::collections::HashMap::new(),
        system_context: std::collections::HashMap::new(),
        user_context_message: None,
        tool_use_options: ToolUseOptions {
            debug: false,
            verbose: false,
            main_loop_model: "haiku".to_string(),
            model_profile: None,
            max_budget_nano_usd: None,
            mcp_clients: vec![],
            is_non_interactive_session: false,
            custom_system_prompt: None,
            append_system_prompt: None,
        },
        fork_context_messages: Vec::new(),
        transcript_path: None,
        generation: 0,
    }
}

#[tokio::test]
async fn compact_snapshot_keeps_request_tool_schema_profile_and_effort() {
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/tmp"),
    )
    .with_cache_safe_slot(slot.clone());
    orch.session.lock().await.model_profile = Some("parent-provider".into());
    orch.set_effort(Some("high".into()));
    let tools = vec![serde_json::json!({
        "name": "Read", "description": "Read text 🧭",
        "input_schema": {"type": "object", "properties": {}},
        "defer_loading": true,
    })];
    orch.save_cache_safe_params(Some("parent prompt"), "claude-sonnet-4-6", &tools)
        .await;
    let captured = slot.get_last().await.unwrap();
    assert_eq!(captured.tools, tools);
    assert_eq!(captured.effort, Some(serde_json::json!("high")));
    assert_eq!(captured.system_prompt.as_ref(), "parent prompt");
    assert_eq!(
        captured.tool_use_options.model_profile.as_deref(),
        Some("parent-provider")
    );
    assert_eq!(
        captured.tool_use_options.main_loop_model,
        "claude-sonnet-4-6"
    );
}

#[tokio::test]
async fn clear_does_not_wait_for_network_extraction_or_commit_stale_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let client = Arc::new(BlockingSideQuery {
        started: Notify::new(),
        release: Notify::new(),
    });
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "haiku".to_string()),
    );
    let handle = Arc::new(SessionMemoryHandle {
        extractor: tokio::sync::Mutex::new(memory::session_memory::SessionMemoryExtractor::new(
            memory::session_memory::SessionMemoryConfig {
                enabled: true,
                initialization_threshold: 0,
                update_threshold: 0,
                minimum_message_tokens_to_init: 0,
                minimum_tokens_between_update: 0,
                tool_calls_between_updates: 0,
                extraction_model: "haiku".to_string(),
            },
        )),
        runner,
        config_home: dir.path().to_path_buf(),
        runtime: Arc::new(TokioRuntime),
        in_flight: std::sync::atomic::AtomicBool::new(false),
        generation: std::sync::atomic::AtomicU64::new(0),
    });
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    slot.save(cache_safe_params()).await;
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_cache_safe_slot(slot)
    .with_session_memory(handle.clone());
    let old_session_id = orch.session.lock().await.session_id.to_string();
    let stale_path = memory::session_memory::session_memory_path(dir.path(), &old_session_id);

    let started = client.started.notified();
    orch.maybe_extract_session_memory().await;
    tokio::time::timeout(std::time::Duration::from_secs(1), started)
        .await
        .expect("background extraction started");

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        lingxi_core::host::OrchestratorHandle::clear_session(&orch),
    )
    .await
    .expect("clear must not wait for the side query")
    .expect("clear succeeds");
    assert!(!stale_path.exists());

    client.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while handle.in_flight.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stale extraction finished");
    assert!(
        !stale_path.exists(),
        "an extraction invalidated by clear must not write its old-session file"
    );
    assert!(!handle.extractor.lock().await.is_initialized());
}
