//! Truncated tool inputs retain the terminal outcome without executing partial JSON.
use futures::StreamExt;
use lingxi_core::host::OutputStream;
use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use orchestrator::streaming_loop::pump_stream;
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, noop_hook_executor, text_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tool_api::ToolRegistry;

#[tokio::test]
async fn incomplete_tool_json_waits_for_terminal_reason_and_never_dispatches() {
    for reason in ["max_tokens", "tool_use", "end_turn"] {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let events = vec![
            message_start("m1", "deepseek-flash"),
            content_block_start_text(0),
            text_delta(0, "Creating the animation."),
            content_block_stop(0),
            content_block_start_tool_use(1, ToolUseId::new(), "Write"),
            input_json_delta(1, "{\"content\":\"unfinished HTML"),
            content_block_stop(1),
            message_delta_stop(reason),
            message_stop(),
        ];
        let result = pump_stream(
            futures::stream::iter(events.into_iter().map(Ok)).boxed(),
            &out,
        )
        .await;

        let turn = result.unwrap();
        assert_eq!(turn.stop_reason.as_deref(), Some(reason));
        assert!(turn.tool_uses.is_empty());
        assert!(matches!(turn.assistant_blocks.as_slice(),
                [ContentBlock::Text { text, .. }] if text == "Creating the animation."));
    }
}

#[tokio::test]
async fn incomplete_tool_json_without_message_stop_preserves_terminal_reason() {
    for reason in [
        "max_tokens",
        "tool_use",
        "end_turn",
        "aborted",
        "insufficient_system_resource",
    ] {
        let out: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
        let events = vec![
            message_start("m1", "deepseek-flash"),
            content_block_start_tool_use(1, ToolUseId::new(), "Write"),
            input_json_delta(1, "{\"content\":\"unfinished HTML"),
            content_block_stop(1),
            message_delta_stop(reason),
        ];
        let result = pump_stream(
            futures::stream::iter(events.into_iter().map(Ok)).boxed(),
            &out,
        )
        .await;
        if matches!(reason, "max_tokens" | "tool_use" | "end_turn") {
            let turn = result.unwrap();
            assert_eq!(turn.stop_reason.as_deref(), Some(reason));
            assert!(turn.tool_uses.is_empty());
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("failed to parse as JSON"));
            assert!(!error.contains("unfinished HTML"));
        }
    }
}

#[test]
fn truncated_tool_arguments_recover_without_execution_or_user_error() {
    // The orchestrator's debug async state machine needs the same enlarged
    // stack used by the existing streaming integration tests.
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    for (reason, mixed) in [("max_tokens",false),("tool_use",false),("end_turn",false),("stop_sequence",false),("max_tokens",true),("tool_use",true),("end_turn",true)] {
                        let cwd = tempfile::tempdir().unwrap();
                        let file = cwd.path().join("pelican-bike.html");
                        let html = "<!doctype html><html><body><svg viewBox='0 0 800 450'></svg></body></html>";
                        let complete_input = serde_json::json!({"file_path":file,"content":html}).to_string();
                        let first_file = cwd.path().join("already-completed.txt");
                        let mut prefix = vec![];
                        if mixed {
                            prefix.extend([
                                content_block_start_tool_use(1, "write-a".into(), "Write"),
                                input_json_delta(1, &serde_json::json!({"file_path":first_file,"content":"preserved result"}).to_string()),
                                content_block_stop(1),
                            ]);
                        }
                        let mut first_turn = vec![message_start("truncated-write", "deepseek-flash")];
                        first_turn.extend(prefix);
                        first_turn.extend(vec![
                                orchestrator::test_support_stream::content_block_start_tool_use(
                                    0,
                                    "incomplete-write".into(),
                                    "Write",
                                ),
                                orchestrator::test_support_stream::input_json_delta(
                                    0,
                                    "{\"file_path\":\"pelican-bike.html\",\"content\":\"<svg",
                                ),
                                content_block_stop(0),
                                message_delta_stop(reason),
                                message_stop(),
                        ]);
                        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
                            first_turn,
                            vec![
                                message_start("complete-write", "deepseek-flash"),
                                content_block_start_tool_use(0, "complete-write".into(), "Write"),
                                input_json_delta(0, &complete_input),
                                content_block_stop(0),
                                message_delta_stop("tool_use"),
                                message_stop(),
                            ],
                            vec![
                                message_start("recovered", "deepseek-flash"),
                                content_block_start_text(0),
                                text_delta(0, "Recovered response"),
                                content_block_stop(0),
                                message_delta_stop("end_turn"),
                                message_stop(),
                            ],
                        ]));
                        let output = Arc::new(MockOutputStream::new());
                        let context = tool_api::test_support::ctx_for_file_tools(
                            tool_api::test_support::make_dummy_fs(),
                            Arc::new(telemetry::AnalyticsBus::new()),
                            vec![cwd.path().to_path_buf()],
                        );
                        let mut tools = ToolRegistry::new();
                        tools.register_builtin(Arc::new(tool_file::FileWriteTool::new(context)));
                        let orch = ConversationOrchestrator::into_shared(
                            ConversationOrchestrator::new_with_streaming(
                                OrchestratorConfig::default(),
                                Arc::new(MockApiClient::new(Vec::new())),
                                streaming.clone(),
                                Arc::new(tools),
                                noop_hook_executor(),
                                Arc::new(NoOpPermissionGate),
                                output,
                                Arc::new(StaticMemoryProvider::empty()),
                                cwd.path().to_path_buf(),
                            ),
                        );
                        orch.run_turn_streaming("create an animation")
                            .await
                            .unwrap();
                        assert_eq!(streaming.captured_calls().await.len(), 3, "{reason}");
                        assert_eq!(std::fs::read_to_string(&file).unwrap(), html);
                        let calls = streaming.captured_calls().await;
                        assert!(calls[2].messages.iter().any(|message| match message { ConversationMessage::User { content, .. } => content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })), _ => false }));
                        let state = orch.session();
                        let session = state.lock().await;
                        assert!(session
                            .history
                            .iter()
                            .any(|message| message.text_content() == "Recovered response"));
                        assert!(!session.history.iter().any(|message| message
                            .text_content()
                            .contains("streaming protocol violation")));
                        assert!(!session.history.iter().any(|message| match message {
                            ConversationMessage::User { content, .. }
                            | ConversationMessage::Assistant { content, .. } =>
                                content.iter().any(|block| matches!(
                                    block,
                                    lingxi_core::types::ContentBlock::ToolUse { id, .. } if id.as_str() == "incomplete-write"
                                )),
                            _ => false,
                        }));
                        assert!(file.exists());
                        if mixed {
                            assert_eq!(std::fs::read_to_string(&first_file).unwrap(), "preserved result");
                            assert!(calls[1].messages.iter().any(|message| match message {
                                ConversationMessage::User { content, .. } => content.iter().any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str()=="write-a")), _=>false,
                            }));
                        }
                    }
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn alternating_length_and_format_failures_share_one_turn_allowance() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let turns = (0..8)
                        .map(|i| {
                            vec![
                                message_start(&format!("failure-{i}"), "deepseek-flash"),
                                content_block_start_tool_use(0, format!("bad-{i}").into(), "Write"),
                                input_json_delta(0, "{\"content\":\"unfinished"),
                                content_block_stop(0),
                                message_delta_stop(if i % 2 == 0 {
                                    "max_tokens"
                                } else {
                                    "tool_use"
                                }),
                                message_stop(),
                            ]
                        })
                        .collect();
                    let streaming = Arc::new(MockStreamingApiClient::with_turns(turns));
                    let cwd = tempfile::tempdir().unwrap();
                    let orch = ConversationOrchestrator::into_shared(
                        ConversationOrchestrator::new_with_streaming(
                            OrchestratorConfig::default(),
                            Arc::new(MockApiClient::new(vec![])),
                            streaming.clone(),
                            Arc::new(ToolRegistry::new()),
                            noop_hook_executor(),
                            Arc::new(NoOpPermissionGate),
                            Arc::new(MockOutputStream::new()),
                            Arc::new(StaticMemoryProvider::empty()),
                            cwd.path().into(),
                        ),
                    );
                    orch.run_turn_streaming("create an animation")
                        .await
                        .unwrap();
                    assert_eq!(streaming.captured_calls().await.len(), 4);
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn budget_retry_reuses_original_request_and_never_appends_partial_json() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let cwd = tempfile::tempdir().unwrap();
            let mut usage = llm_runtime::ExecutionUsage::from_counts(
                lingxi_llm_client::protocol::Usage { input_tokens: 1000, output_tokens: 65536, ..Default::default() });
            usage.provider_metadata = serde_json::json!({"llm_client":{"output_budget":{"can_repeat_original":true,"source":"provider_default","provider_default":65536}}});
            let first = vec![message_start("failed", "deepseek-flash"),
                content_block_start_tool_use(0, ToolUseId::from("partial"), "Write"),
                input_json_delta(0, "{\"content\":\"partial private source"), content_block_stop(0),
                orchestrator::test_support::message_delta_stop_with_usage("max_tokens", usage), message_stop()];
            let second = vec![message_start("final", "deepseek-flash"), content_block_start_text(0),
                text_delta(0, "done"), content_block_stop(0), message_delta_stop("end_turn"), message_stop()];
            let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![first, second]));
            let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig { model:"deepseek-flash".into(), ..Default::default() },
                Arc::new(MockApiClient::new(vec![]).with_recovery_budget(131072)), streaming.clone(),
                Arc::new(ToolRegistry::new()), noop_hook_executor(), Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()), Arc::new(StaticMemoryProvider::empty()), cwd.path().into()));
            orch.run_turn_streaming("create the pelican animation").await.unwrap();
            let calls = streaming.captured_calls().await;
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].output_budget, None);
            assert_eq!(calls[1].output_budget, Some(131072));
            assert_eq!(calls[0].model, calls[1].model);
            assert_eq!(calls[0].profile, calls[1].profile);
            assert_eq!(serde_json::to_value(&calls[0].messages).unwrap(), serde_json::to_value(&calls[1].messages).unwrap());
            assert_eq!(calls[0].system, calls[1].system);
            assert_eq!(calls[0].tools, calls[1].tools);
            assert!(!serde_json::to_string(&calls[1].messages).unwrap().contains("partial private source"));
        });
    }).unwrap().join().unwrap();
}
