//! Public-library checks for isolated Side Chat model context and controls.
use async_trait::async_trait;
use lingxi_core::{
    host::{CancellationToken, OrchestratorHandle},
    types::{ConversationMessage, MessageId},
};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig, StreamingApiClient};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

struct PermissionInputFixture;

#[async_trait]
impl Tool for PermissionInputFixture {
    fn name(&self) -> &str {
        "Bash"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<Utf16JsonProjection> =
            once_cell::sync::Lazy::new(|| {
                tool_api::tool_trait::generated_builtin_schema(&json!({"type": "object"}))
            });
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }
    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Bash".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        "".into()
    }
    async fn call(
        &self,
        _input: Utf16JsonProjection,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: Utf16JsonProjection::from_serde_value(&json!({"ok": true}))
                .expect("generated fixture"),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

struct PendingStream(Arc<Notify>);
#[async_trait]
impl StreamingApiClient for PendingStream {
    async fn stream(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _: Vec<ConversationMessage>,
        _: Vec<llm_runtime::ToolDeclaration>,
        _: &str,
        _: bool,
        _: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.0.notify_one();
        futures::future::pending().await
    }
}
#[tokio::test]
async fn fixed_legal_busy_context_ready_models_and_global_flags() {
    lingxi_core::host::session_flags::set_non_interactive_session(false);
    lingxi_core::host::session_flags::set_tool_search_enabled(true);
    let entered = Arc::new(Notify::new());
    let mut config = OrchestratorConfig::default();
    config.model = "keyless-current".into();
    config.interactive_session = false;
    let parent = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_side_chat(
            config,
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(PendingStream(entered.clone())),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_side_chat_model_refs(vec!["keyless-current".into()]),
    );
    assert!(!lingxi_core::host::session_flags::is_non_interactive_session());
    assert!(lingxi_core::host::session_flags::tool_search_enabled());
    assert!(parent.switch_model("keyless-current", None).await.is_ok());
    assert!(parent
        .select_model(lingxi_core::host::ModelSelectionRequest::Select {
            source: "unconnected-model".into(),
            route_hint: Some(lingxi_core::host::ModelRouteAnchor {
                model: "unconnected-model".into(),
                profile: Some("other-profile".into()),
            }),
            origin: lingxi_core::host::ModelSelectionOrigin::Command,
            validation: lingxi_core::host::ModelValidationMode::ServerProbeDisabled,
        })
        .await
        .is_err());
    assert_eq!(
        parent.list_available_models().await,
        vec!["keyless-current"]
    );
    let user:ConversationMessage=serde_json::from_value(serde_json::json!({"role":"user","id":MessageId::new(),"content":[{"type":"text","text":"hello"}]})).unwrap();
    let assistant:ConversationMessage=serde_json::from_value(serde_json::json!({"role":"assistant","id":MessageId::new(),"content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{}}],"stop_reason":"tool_use"})).unwrap();
    let result:ConversationMessage=serde_json::from_value(serde_json::json!({"role":"user","id":MessageId::new(),"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"done","is_error":false}]})).unwrap();
    parent.session().lock().await.history = vec![user.clone(), assistant.clone()];
    assert_eq!(
        parent.side_chat_parent_snapshot().await.history,
        vec![user.clone()]
    );
    parent.session().lock().await.history.push(result.clone());
    let fixed = parent.side_chat_parent_snapshot().await;
    assert_eq!(fixed.history, vec![user, assistant, result]);
    let cancel = CancellationToken::new();
    let running = parent.clone();
    let token = cancel.clone();
    let task =
        tokio::spawn(async move { running.run_turn_streaming_with_cancel("busy", token).await });
    tokio::time::timeout(Duration::from_secs(15), entered.notified())
        .await
        .unwrap();
    let busy = tokio::time::timeout(Duration::from_secs(1), parent.side_chat_parent_snapshot())
        .await
        .expect("snapshot must not await parent foreground turn");
    assert_eq!(busy.config.model, "keyless-current");
    assert_eq!(fixed.history.len(), 3);
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!lingxi_core::host::session_flags::is_non_interactive_session());
    assert!(lingxi_core::host::session_flags::tool_search_enabled());
}

#[test]
fn side_payloads_do_not_enter_enabled_otel_or_tracing_exports() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "payload_export_child", "--nocapture"])
        .env("SIDE_CHAT_EXPORT_CHILD", "1")
        .env(telemetry::otel::config::ENV_ENABLE_TELEMETRY, "1")
        .env("OTEL_LOGS_EXPORTER", "console")
        .env("OTEL_METRICS_EXPORTER", "none")
        .env("OTEL_TRACES_EXPORTER", "none")
        .env("OTEL_LOG_USER_PROMPTS", "1")
        .env("OTEL_LOG_ASSISTANT_RESPONSES", "1")
        .env("OTEL_LOG_TOOL_DETAILS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The OTEL allowlist strips prompt and tool input even when the legacy
    // body-capture flags are enabled. Assert the main turn's diagnostic events
    // actually arrived, and that the side turn emitted no extra copies.
    assert_eq!(
        exported.matches("EventName: \"user_prompt\"").count(),
        1,
        "{exported}"
    );
    assert_eq!(
        exported.matches("EventName: \"tool_decision\"").count(),
        1,
        "{exported}"
    );
    assert_eq!(
        exported
            .matches("EventName: \"assistant_response\"")
            .count(),
        2,
        "{exported}"
    );
    assert!(
        exported.contains("MAIN_RESPONSE_EXPORT_MARKER"),
        "{exported}"
    );
    assert!(
        !exported.contains("MAIN_PROMPT_EXPORT_MARKER"),
        "{exported}"
    );
    assert!(!exported.contains("MAIN_TOOL_EXPORT_MARKER"), "{exported}");
    assert!(!exported.contains("SIDE_TOOL_PRIVATE_MARKER"), "{exported}");
    assert!(
        !exported.contains("SIDE_PROMPT_PRIVATE_MARKER"),
        "{exported}"
    );
    assert!(
        !exported.contains("SIDE_RESPONSE_PRIVATE_MARKER"),
        "{exported}"
    );
}

#[tokio::test]
async fn payload_export_child() {
    if std::env::var_os("SIDE_CHAT_EXPORT_CHILD").is_none() {
        return;
    }
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .init();
    let guard = telemetry::otel::install_process("side-chat-test", false);
    for side in [false, true] {
        let response = orchestrator::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: if side {
                    "SIDE_RESPONSE_PRIVATE_MARKER"
                } else {
                    "MAIN_RESPONSE_EXPORT_MARKER"
                }
                .into(),
                citations: None,
                cache_control: None,
            }],
            Some("end_turn"),
        );
        let tool_response = orchestrator::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::ToolCall {
                id: lingxi_core::types::ToolUseId::new().to_string(),
                name: "Bash".into(),
                input: lingxi_llm_client::protocol::JsonPayload::from_generated_value(&json!({"command":if side {"SIDE_TOOL_PRIVATE_MARKER"} else {"MAIN_TOOL_EXPORT_MARKER"}})).expect("generated fixture"),
            }],
            Some("tool_use"),
        );
        let api = Arc::new(MockApiClient::new(vec![tool_response, response]));
        let mut tools = tool_api::ToolRegistry::new();
        tools.register_builtin(Arc::new(PermissionInputFixture));
        let tools = Arc::new(tools);
        let engine = if side {
            ConversationOrchestrator::new_side_chat(
                OrchestratorConfig::default(),
                api,
                Arc::new(orchestrator::test_support::MockStreamingApiClient::empty()),
                tools,
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
        } else {
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                api,
                tools,
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
        };
        engine
            .run_turn(if side {
                "SIDE_PROMPT_PRIVATE_MARKER"
            } else {
                "MAIN_PROMPT_EXPORT_MARKER"
            })
            .await
            .unwrap();
    }
    drop(guard); // Flush the actual console exporter before parent inspection.
}

struct StalledSummary(Arc<Notify>);
#[async_trait]
impl sidequery::SideQueryClient for StalledSummary {
    async fn query(
        &self,
        _: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.0.notify_one();
        futures::future::pending().await
    }
}

#[tokio::test]
async fn cancelled_proactive_and_reactive_summaries_finish_without_committing_history() {
    for reactive in [false, true] {
        let entered = Arc::new(Notify::new());
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(sidequery::ForkedAgentRunner::new().with_side_query_client(
            Arc::new(StalledSummary(entered.clone())),
            "claude-opus-4-7".into(),
        ));
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            if reactive { u64::MAX } else { 1 },
        ));
        let api = Arc::new(MockApiClient::new(vec![]));
        api.set_fail_with(Some(llm_runtime::LlmError::ContextOverflow {
            token_gap: 0,
        }));
        let output = Arc::new(MockOutputStream::new());
        let engine = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_side_chat(
                OrchestratorConfig::default(),
                api,
                Arc::new(
                    orchestrator::test_support::MockStreamingApiClient::with_open_error(
                        llm_runtime::LlmError::ContextOverflow { token_gap: 0 },
                        vec![],
                    ),
                ),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
            .with_compaction(compactor)
            .with_cache_safe_slot(slot),
        );
        engine.session().lock().await.history = (0..20)
            .flat_map(|index| {
                let user = ConversationMessage::user(MessageId::new(), format!("user round {index} {}", "padding ".repeat(200)));
                let assistant: ConversationMessage = serde_json::from_value(json!({
                    "role":"assistant", "id":MessageId::new(),
                    "content":[{"type":"text","text":format!("assistant round {index} {}", "answer ".repeat(200))}],
                    "stop_reason":"end_turn"
                })).unwrap();
                [user, assistant]
            })
            .collect();
        let cancel = CancellationToken::new();
        let running = engine.clone();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            running
                .run_turn_streaming_with_cancel("pending summary", token)
                .await
        });
        let path = if reactive { "reactive" } else { "proactive" };
        let admission = tokio::time::timeout(Duration::from_secs(15), entered.notified()).await;
        assert!(
            admission.is_ok(),
            "{path} summarizer must enter before cancellation; phases={:?}; turn_finished={}",
            output.compaction_phase_snapshot().await,
            task.is_finished()
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .unwrap_or_else(|error| {
                panic!("{path} Stop must finish stalled summarization: {error}")
            })
            .unwrap()
            .unwrap();
        assert!(output
            .compaction_phase_snapshot()
            .await
            .iter()
            .any(|phase| phase == "cancelled"));
        assert!(engine
            .side_chat_parent_snapshot()
            .await
            .history
            .iter()
            .all(|message| !matches!(
                message,
                ConversationMessage::User {
                    is_compact_summary: true,
                    ..
                }
            )));
    }
}
