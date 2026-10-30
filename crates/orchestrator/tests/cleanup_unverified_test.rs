use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ToolUseId;
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_tool_use, content_block_stop, message_delta_stop, message_start,
    message_stop, MockStreamingApiClient,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig, OrchestratorError};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
    ValidationError,
};

struct CleanupTool {
    started: Arc<tokio::sync::Notify>,
    with_output: bool,
    with_batch: bool,
    wait_for_cancel: bool,
}

#[async_trait]
impl Tool for CleanupTool {
    fn name(&self) -> &str {
        "CleanupTool"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::LazyLock<Utf16JsonProjection> = std::sync::LazyLock::new(|| {
            tool_api::tool_trait::generated_builtin_schema(&json!({"type":"object"}))
        });
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }
    fn await_cleanup_on_interrupt(&self) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "test".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "test".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.started.notify_one();
        if self.wait_for_cancel {
            ctx.cancel
                .expect("turn cancellation reaches the tool")
                .cancelled()
                .await;
        }
        tokio::task::yield_now().await;
        if self.with_output {
            let output =
                tool_api::UnverifiedCleanupOutput::new(tool_api::UnverifiedCleanupOutputParts {
                    session_id: Some("raw-session".into()),
                    stdout: "before-and-after-stop".into(),
                    stderr: "stderr-tail".into(),
                    stdout_truncated: true,
                    stderr_truncated: false,
                    stdout_eof: false,
                    stderr_eof: true,
                    exit_code: if self.wait_for_cancel { None } else { Some(0) },
                    timed_out: true,
                    termination_requested: Some(true),
                    tree_status: if self.wait_for_cancel {
                        tool_api::ProcessTreeStatus::VerifiedReaped
                    } else {
                        tool_api::ProcessTreeStatus::Unverified
                    },
                    cleanup_detail: "native close failed".into(),
                    original_error: Some("read failed".into()),
                });
            if self.with_batch {
                let mut second = output.parts().clone();
                second.session_id = Some("second-session".into());
                second.stdout = "second-stdout".into();
                second.original_error = Some("second-error".into());
                let malformed =
                    tool_api::ReportedShellPayload::new(tool_api::ReportedShellPayloadParts {
                        stdout: tool_api::ReportedShellStream::Present {
                            utf16: vec![0xd800, 65],
                            host_truncated: false,
                        },
                        stderr: tool_api::ReportedShellStream::Missing,
                        exit_code: tool_api::ReportedExitCode::OutOfRange {
                            bits: 2_147_483_648f64.to_bits(),
                        },
                        combined_truncated: Some(true),
                        timed_out: None,
                        tool_result_is_error: false,
                        payload_is_error: None,
                        interrupted: None,
                    });
                Err(ToolError::CleanupUnverifiedBatch(vec![
                    tool_api::CleanupUnverifiedReason::Output(Arc::new(output)),
                    tool_api::CleanupUnverifiedReason::Output(Arc::new(
                        tool_api::UnverifiedCleanupOutput::new(second),
                    )),
                    tool_api::CleanupUnverifiedReason::ReportedPayload(Arc::new(malformed)),
                ]))
            } else {
                Err(ToolError::CleanupUnverifiedOutput(Box::new(output)))
            }
        } else {
            Err(ToolError::CleanupUnverified("native close failed".into()))
        }
    }
}

fn registry(
    started: Arc<tokio::sync::Notify>,
    with_output: bool,
    wait_for_cancel: bool,
    with_batch: bool,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(CleanupTool {
        started,
        with_output,
        wait_for_cancel,
        with_batch,
    }));
    Arc::new(registry)
}

fn assert_cleanup_reason(
    reason: tool_api::CleanupUnverifiedReason,
    with_output: bool,
    expected_exit_code: Option<i32>,
    with_batch: bool,
) {
    if with_batch {
        let tool_api::CleanupUnverifiedReason::Multiple(mut reasons) = reason else {
            panic!("multiple cleanup observations were discarded");
        };
        assert_eq!(reasons.len(), 3);
        let malformed = reasons.pop().unwrap();
        let tool_api::CleanupUnverifiedReason::ReportedPayload(malformed) = malformed else {
            panic!("malformed shell observation was discarded");
        };
        assert_eq!(
            malformed.parts().stdout,
            tool_api::ReportedShellStream::Present {
                utf16: vec![0xd800, 65],
                host_truncated: false
            }
        );
        assert_eq!(
            malformed.parts().stderr,
            tool_api::ReportedShellStream::Missing
        );
        assert_eq!(
            malformed.parts().exit_code,
            tool_api::ReportedExitCode::OutOfRange {
                bits: 2_147_483_648f64.to_bits()
            }
        );
        assert_eq!(malformed.parts().combined_truncated, Some(true));
        let second = reasons.pop().unwrap();
        let tool_api::CleanupUnverifiedReason::Output(second) = second else {
            panic!("second cleanup observation was discarded");
        };
        assert_eq!(second.parts().session_id.as_deref(), Some("second-session"));
        assert_eq!(second.parts().stdout, "second-stdout");
        assert_eq!(
            second.parts().original_error.as_deref(),
            Some("second-error")
        );
        assert_cleanup_reason(reasons.pop().unwrap(), true, expected_exit_code, false);
        return;
    }
    if with_output {
        let tool_api::CleanupUnverifiedReason::Output(output) = reason else {
            panic!("cleanup output was discarded");
        };
        let parts = output.parts();
        assert_eq!(parts.session_id.as_deref(), Some("raw-session"));
        assert_eq!(parts.stdout, "before-and-after-stop");
        assert_eq!(parts.stderr, "stderr-tail");
        assert!(parts.stdout_truncated);
        assert!(!parts.stderr_truncated);
        assert!(!parts.stdout_eof);
        assert!(parts.stderr_eof);
        assert_eq!(parts.exit_code, expected_exit_code);
        assert!(parts.timed_out);
        assert_eq!(parts.termination_requested, Some(true));
        assert_eq!(
            parts.tree_status,
            if expected_exit_code.is_none() {
                tool_api::ProcessTreeStatus::VerifiedReaped
            } else {
                tool_api::ProcessTreeStatus::Unverified
            }
        );
        assert_eq!(parts.cleanup_detail, "native close failed");
        assert_eq!(parts.original_error.as_deref(), Some("read failed"));
    } else {
        assert!(
            matches!(reason, tool_api::CleanupUnverifiedReason::Message(detail) if detail == "native close failed")
        );
    }
}

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .name("cleanup-unverified".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack test thread")
        .join()
        .expect("cleanup test panicked");
}

#[test]
fn batched_cancel_preserves_cleanup_unverified() {
    run_with_large_stack(|| batched_cleanup_check(false, true, false));
}

#[test]
fn batched_cancel_preserves_unverified_cleanup_output() {
    run_with_large_stack(|| batched_cleanup_check(true, true, false));
}

#[test]
fn batched_unverified_output_does_not_continue_without_cancel() {
    run_with_large_stack(|| batched_cleanup_check(true, false, false));
}

#[test]
fn batched_multiple_unverified_outputs_do_not_continue() {
    run_with_large_stack(|| batched_cleanup_check(true, false, true));
}

async fn batched_cleanup_check(with_output: bool, cancel_turn: bool, with_batch: bool) {
    let started = Arc::new(tokio::sync::Notify::new());
    let response = mock_message_response(
        vec![llm_runtime::ContentBlock::ToolCall {
            id: "toolu_cleanup".into(),
            name: "CleanupTool".into(),
            input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(&json!(
                {}
            ))
            .unwrap(),
        }],
        Some("tool_use"),
    );
    let api = Arc::new(MockApiClient::new(vec![response]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        registry(started.clone(), with_output, cancel_turn, with_batch),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (result, ()) = tokio::join!(orch.run_turn_with_cancel("test", cancel.clone()), async {
            started.notified().await;
            if cancel_turn {
                cancel.cancel();
            }
        });
        result
    })
    .await
    .expect("turn and cleanup complete");
    let Err(OrchestratorError::CleanupUnverified(detail)) = result else {
        panic!("cleanup failure did not stop the batched turn");
    };
    assert_cleanup_reason(
        detail,
        with_output,
        if cancel_turn { None } else { Some(0) },
        with_batch,
    );
    assert_eq!(
        api.captured_requests().await.len(),
        1,
        "cleanup failure must not query again"
    );
}

#[test]
fn streaming_cancel_preserves_cleanup_unverified() {
    run_with_large_stack(|| streaming_cleanup_check(false, true, false));
}

#[test]
fn streaming_cancel_preserves_unverified_cleanup_output() {
    run_with_large_stack(|| streaming_cleanup_check(true, true, false));
}

#[test]
fn streaming_unverified_output_does_not_continue_without_cancel() {
    run_with_large_stack(|| streaming_cleanup_check(true, false, false));
}

#[test]
fn streaming_multiple_unverified_outputs_survive_cancel() {
    run_with_large_stack(|| streaming_cleanup_check(true, true, true));
}

async fn streaming_cleanup_check(with_output: bool, cancel_turn: bool, with_batch: bool) {
    let started = Arc::new(tokio::sync::Notify::new());
    let script = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, ToolUseId::new(), "CleanupTool"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let stream_api = Arc::new(MockStreamingApiClient::with_turns(vec![script]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        stream_api.clone(),
        registry(started.clone(), with_output, cancel_turn, with_batch),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    ));
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (result, ()) = tokio::join!(
            orch.run_turn_streaming_with_cancel("test", cancel.clone()),
            async {
                started.notified().await;
                if cancel_turn {
                    cancel.cancel();
                }
            }
        );
        result
    })
    .await
    .expect("streaming turn and cleanup complete");
    let Err(OrchestratorError::CleanupUnverified(detail)) = result else {
        panic!("cleanup failure did not stop the streaming turn");
    };
    assert_cleanup_reason(
        detail,
        with_output,
        if cancel_turn { None } else { Some(0) },
        with_batch,
    );
    assert_eq!(
        stream_api.captured_calls().await.len(),
        1,
        "cleanup failure must not query again"
    );
}
