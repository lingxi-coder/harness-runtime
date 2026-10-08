//! Tests for `streaming_executor.rs`, extracted from inline `#[cfg(test)]` blocks.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TOOL_CONCURRENCY_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn tool_concurrency_env_guard() -> std::sync::MutexGuard<'static, ()> {
        TOOL_CONCURRENCY_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn status_enum_roundtrips() {
        assert_eq!(ToolStatus::Queued, ToolStatus::Queued);
        assert_ne!(ToolStatus::Queued, ToolStatus::Yielded);
    }

    #[test]
    fn streaming_fallback_synthetic() {
        let block = synthetic_error_block(ToolUseId::new(), AbortReason::StreamingFallback);
        let ContentBlock::ToolResult { content, .. } = block else {
            panic!()
        };
        assert_eq!(
            content,
            "<tool_use_error>Error: Streaming fallback - tool execution discarded</tool_use_error>"
        );
    }

    // ============================================================================
    // Task 6: StreamingToolExecutor::add_tool tests
    // ============================================================================

    use crate::OrchestratorConfig;
    use crate::conversation::ConversationOrchestrator;
    use crate::test_support::{
        MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
        StaticMemoryProvider, mock_message_response, noop_hook_executor,
    };
    use crate::test_support_stream::{
        content_block_start_text, content_block_start_tool_use, content_block_stop,
        input_json_delta, message_start, message_stop, text_delta,
    };
    use async_trait::async_trait;
    use futures::stream::{self, StreamExt};
    use lingxi_core::types::MessageId;
    use llm_runtime::{HistoryEvent, LlmError};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Notify;
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::registry::ToolRegistry;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
        ValidationError,
    };

    /// Build an orchestrator with an EMPTY tool registry. Used to exercise the
    /// unknown-tool short-circuit path.
    fn orch_empty() -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    fn background_context_orch(sink: Arc<MockOutputStream>) -> Arc<ConversationOrchestrator> {
        ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            sink,
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ))
    }

    /// A minimal concurrency-safe tool for the "known tool" path test.
    #[derive(Default)]
    struct SafeTool {
        call_entries: Option<Arc<std::sync::atomic::AtomicUsize>>,
    }

    #[async_trait]
    impl Tool for SafeTool {
        fn name(&self) -> &str {
            "SafeTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
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
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            "safe-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            if let Some(call_entries) = &self.call_entries {
                call_entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    #[derive(Clone)]
    struct CapturedW1Context {
        messages: Vec<lingxi_core::types::ConversationMessage>,
        assistant_message: Option<lingxi_core::types::ConversationMessage>,
        same_turn_tool_uses: Vec<ContentBlock>,
        tool_use_id: Option<lingxi_core::types::ToolUseId>,
        assistant_message_id: Option<MessageId>,
        session_ptr: Option<usize>,
        registry_ptr: Option<usize>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        cwd: Option<PathBuf>,
        tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy,
        trusted_effective_permission_mode: Option<String>,
        has_instruction_context: bool,
        main_loop_model: String,
        verbose: bool,
    }

    struct W1ContextRecordingTool {
        captured: Arc<Mutex<Vec<CapturedW1Context>>>,
        first_started: Arc<Notify>,
        release_first: Arc<Notify>,
        call_returned: Arc<Notify>,
        return_context_layer: bool,
    }

    struct DetachedContinuationTool {
        started: tokio::sync::mpsc::UnboundedSender<String>,
        dropped: tokio::sync::mpsc::UnboundedSender<String>,
        release_old: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        continue_old: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        continued: tokio::sync::mpsc::UnboundedSender<String>,
        finish_old: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        observed_models: Arc<Mutex<Vec<String>>>,
        modifier_applications: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct CallDropGuard {
        step: String,
        dropped: tokio::sync::mpsc::UnboundedSender<String>,
    }

    impl Drop for CallDropGuard {
        fn drop(&mut self) {
            let _ = self.dropped.send(self.step.clone());
        }
    }

    #[async_trait]
    impl Tool for DetachedContinuationTool {
        fn name(&self) -> &str {
            "DetachedContinuation"
        }

        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object" }));
            &SCHEMA
        }

        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }

        fn max_result_size_chars(&self) -> usize {
            1024
        }

        fn is_concurrency_safe(&self, input: &serde_json::Value) -> bool {
            input.get("safe").and_then(serde_json::Value::as_bool) == Some(true)
        }

        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }

        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "detached continuation test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }

        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "detached continuation".into()
        }

        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }

        async fn call(
            &self,
            input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let step = input
                .get("step")
                .and_then(serde_json::Value::as_str)
                .expect("detached call has a step")
                .to_owned();
            self.started
                .send(step.clone())
                .expect("test remains subscribed to W1 entry");
            self.observed_models
                .lock()
                .unwrap()
                .push(ctx.options.main_loop_model.clone());
            let _drop_guard = (step == "old").then(|| CallDropGuard {
                step: step.clone(),
                dropped: self.dropped.clone(),
            });

            if step == "old" {
                // Deliberately ignore the cancellation token, modeling a
                // non-cooperative promise that continues after Native discard.
                let release = self
                    .release_old
                    .lock()
                    .unwrap()
                    .take()
                    .expect("old W1 call has a release gate");
                let _ = release.await;
                let continue_old = self
                    .continue_old
                    .lock()
                    .unwrap()
                    .take()
                    .expect("old W1 call has a continuation gate");
                let _ = continue_old.await;
                self.continued
                    .send(step.clone())
                    .expect("detached call remains subscribed");
                let finish_old = self
                    .finish_old
                    .lock()
                    .unwrap()
                    .take()
                    .expect("old W1 call has a final gate");
                let _ = finish_old.await;
            }

            let modifier_applications = Arc::clone(&self.modifier_applications);
            let model = if step == "old" {
                "old-generation-layer"
            } else {
                "new-generation-layer"
            }
            .to_owned();
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "step": step }),
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: Some(Box::new(move |mut context: ToolUseContext| {
                    modifier_applications.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    context.options.main_loop_model = model;
                    context
                })),
                is_error: false,
                mcp_meta: (step == "old").then(|| {
                    json!({
                        "fence_probe": "old-generation",
                        "_meta": { "claude/endTurn": true },
                    })
                }),
            })
        }
    }

    #[async_trait]
    impl Tool for W1ContextRecordingTool {
        fn name(&self) -> &str {
            "W1Capture"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object" }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, input: &serde_json::Value) -> bool {
            input.get("hold").and_then(|value| value.as_bool()) != Some(true)
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            "W1 capture".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let captured = CapturedW1Context {
                messages: ctx.messages.clone(),
                assistant_message: ctx.assistant_message.clone(),
                same_turn_tool_uses: ctx.same_turn_tool_uses.clone(),
                tool_use_id: ctx.tool_use_id.clone(),
                assistant_message_id: ctx.assistant_message_id,
                session_ptr: ctx
                    .session
                    .as_ref()
                    .map(|session| Arc::as_ptr(session) as usize),
                registry_ptr: ctx
                    .subagent_registry
                    .as_ref()
                    .map(|registry| Arc::as_ptr(registry) as usize),
                cancel_token: ctx.cancel.clone(),
                cwd: ctx.cwd.clone(),
                tool_execution_policy: ctx.tool_execution_policy,
                trusted_effective_permission_mode: ctx.trusted_effective_permission_mode.clone(),
                has_instruction_context: ctx.instruction_context.is_some(),
                main_loop_model: ctx.options.main_loop_model.clone(),
                verbose: ctx.options.verbose,
            };
            self.captured.lock().unwrap().push(captured.clone());
            let held = input.get("hold").and_then(|value| value.as_bool()) == Some(true);
            if held {
                self.first_started.notify_one();
                self.release_first.notified().await;
            }
            let layered_model = input
                .get("layered_model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("layered-model")
                .to_owned();
            let layered_cwd = input
                .get("layered_cwd")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from);
            let layered_permission_mode = input
                .get("layered_permission_mode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let context_modifier = self.return_context_layer.then(|| {
                let expected = captured;
                Box::new(move |mut context: ToolUseContext| {
                    assert_eq!(context.messages, expected.messages);
                    assert!(context.assistant_message.is_none());
                    assert!(context.same_turn_tool_uses.is_empty());
                    assert!(context.tool_use_id.is_none());
                    assert!(context.assistant_message_id.is_none());
                    assert_eq!(
                        context
                            .session
                            .as_ref()
                            .map(|session| Arc::as_ptr(session) as usize),
                        expected.session_ptr
                    );
                    assert_eq!(
                        context
                            .subagent_registry
                            .as_ref()
                            .map(|registry| Arc::as_ptr(registry) as usize),
                        expected.registry_ptr
                    );
                    assert_eq!(context.cwd, expected.cwd);
                    assert_eq!(
                        context.instruction_context.is_some(),
                        expected.has_instruction_context
                    );
                    assert_eq!(
                        context.tool_execution_policy,
                        expected.tool_execution_policy
                    );
                    assert_eq!(
                        context.trusted_effective_permission_mode,
                        expected.trusted_effective_permission_mode
                    );
                    assert_eq!(context.options.main_loop_model, expected.main_loop_model);
                    assert_eq!(context.options.verbose, expected.verbose);
                    context.options.main_loop_model = layered_model;
                    context.options.verbose = true;
                    if let Some(cwd) = layered_cwd {
                        context.cwd = Some(cwd);
                    }
                    if let Some(permission_mode) = layered_permission_mode {
                        context.trusted_effective_permission_mode = Some(permission_mode);
                    }
                    context
                }) as tool_api::ContextModifier
            });
            let result = ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier,
                is_error: false,
                mcp_meta: None,
            };
            self.call_returned.notify_one();
            Ok(result)
        }
    }

    struct ProgressFloodTool {
        started: Arc<Notify>,
        queue_filled: Arc<Notify>,
        blocked_send_started: Arc<Notify>,
        blocked_send_finished: Arc<Notify>,
        release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    }

    #[async_trait]
    impl Tool for ProgressFloodTool {
        fn name(&self) -> &str {
            "ProgressFlood"
        }

        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object" }));
            &SCHEMA
        }

        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
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
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "progress-reset regression".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }

        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "progress flood".into()
        }

        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }

        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolUseContext,
            tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let tool_use_id = ctx.tool_use_id.expect("owned W1 supplies tool_use_id");
            let progress = || tool_api::progress::ToolProgress {
                tool_use_id: tool_use_id.clone(),
                data: json!({ "subagent_activity": "progress" }),
            };
            self.started.notify_one();
            tx.send(progress())
                .await
                .expect("progress consumer remains attached");
            for _ in 0..64 {
                tx.send(progress())
                    .await
                    .expect("buffered progress remains accepted");
            }
            self.queue_filled.notify_one();
            self.blocked_send_started.notify_one();
            tx.send(progress())
                .await
                .expect("reset drains the bounded progress queue");
            self.blocked_send_finished.notify_one();
            let release = self
                .release
                .lock()
                .unwrap()
                .take()
                .expect("test owns the progress tool release gate");
            let _ = release.await;
            Ok(ToolCallResult::from_data(json!({ "completed": true })))
        }
    }

    struct SinkFutureDropGuard(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for SinkFutureDropGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct BlockingProgressSink {
        entered: Arc<Notify>,
        future_dropped: Arc<std::sync::atomic::AtomicBool>,
        tool_calls: Arc<std::sync::atomic::AtomicUsize>,
        tool_results: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl lingxi_core::host::OutputStream for BlockingProgressSink {
        async fn emit_text(&self, _text: &str, _utf16_code_units: Option<&[u16]>) {}

        async fn emit_tool_call(&self, _id: &ToolUseId, _tool: &str, _input: &serde_json::Value, _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>) {
            self.tool_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        async fn emit_tool_result(
            &self,
            _id: &ToolUseId,
            _tool: &str,
            _model_text: &str,
            _result: &serde_json::Value,
         _projection: Option<&lingxi_core::host::ToolResultProjection>) {
            self.tool_results
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        async fn emit_end_turn(
            &self,
            _stop_reason: &str,
            _cost: &lingxi_core::host::orchestrator::CostSnapshot,
        ) {
        }

        async fn emit_subagent_activity(&self, _text: &str) {
            let _dropped = SinkFutureDropGuard(Arc::clone(&self.future_dropped));
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
    }

    struct ProgressFloodFixture {
        orch: Arc<ConversationOrchestrator>,
        started: Arc<Notify>,
        queue_filled: Arc<Notify>,
        blocked_send_started: Arc<Notify>,
        blocked_send_finished: Arc<Notify>,
        release: tokio::sync::oneshot::Sender<()>,
        sink: Arc<BlockingProgressSink>,
    }

    fn progress_flood_fixture() -> ProgressFloodFixture {
        let started = Arc::new(Notify::new());
        let queue_filled = Arc::new(Notify::new());
        let blocked_send_started = Arc::new(Notify::new());
        let blocked_send_finished = Arc::new(Notify::new());
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let sink = Arc::new(BlockingProgressSink {
            entered: Arc::new(Notify::new()),
            future_dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool_results: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(ProgressFloodTool {
            started: Arc::clone(&started),
            queue_filled: Arc::clone(&queue_filled),
            blocked_send_started: Arc::clone(&blocked_send_started),
            blocked_send_finished: Arc::clone(&blocked_send_finished),
            release: Arc::new(Mutex::new(Some(release_rx))),
        }) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            sink.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        ProgressFloodFixture {
            orch,
            started,
            queue_filled,
            blocked_send_started,
            blocked_send_finished,
            release,
            sink,
        }
    }

    struct ParkedPermissionTool {
        first_permission_waiting: Arc<Notify>,
        release_first_permission: Arc<Notify>,
        first_permission_finished: Arc<Notify>,
        first_saw_cancellation: Arc<std::sync::atomic::AtomicBool>,
        second_call_started: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for ParkedPermissionTool {
        fn name(&self) -> &str {
            "ParkedPermission"
        }

        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object" }));
            &SCHEMA
        }

        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
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
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn check_permissions(
            &self,
            input: &serde_json::Value,
            ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            if input.get("step").and_then(|step| step.as_str()) == Some("first") {
                self.first_permission_waiting.notify_one();
                let cancellation = ctx
                    .cancel
                    .as_ref()
                    .expect("owned streaming dispatch supplies cancellation")
                    .clone();
                self.release_first_permission.notified().await;
                self.first_saw_cancellation.store(
                    cancellation.is_cancelled(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                self.first_permission_finished.notify_one();
                return permission::PermissionResult::Deny {
                    reason: permission::PermissionDecisionReason::Other {
                        reason: "scheduler reset".into(),
                    },
                    explanation: None,
                    metadata: permission::result::PermissionMetadata::default(),
                };
            }

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
            _opts: &DescriptionOptions,
        ) -> String {
            "parked permission tool".into()
        }

        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }

        async fn call(
            &self,
            input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            if input.get("step").and_then(|step| step.as_str()) == Some("second") {
                self.second_call_started.notify_one();
            }
            Ok(ToolCallResult::from_data(json!({ "content": "ok" })))
        }
    }

    struct ActorQueueTool {
        started: tokio::sync::mpsc::UnboundedSender<String>,
        releases: Arc<Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Receiver<()>>>>,
    }

    #[async_trait]
    impl Tool for ActorQueueTool {
        fn name(&self) -> &str {
            "ActorQueue"
        }

        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object" }));
            &SCHEMA
        }

        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }

        fn max_result_size_chars(&self) -> usize {
            1024
        }

        fn is_concurrency_safe(&self, input: &serde_json::Value) -> bool {
            input.get("safe").and_then(serde_json::Value::as_bool) == Some(true)
        }

        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }

        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "actor queue test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }

        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "actor queue".into()
        }

        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }

        async fn call(
            &self,
            input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let step = input
                .get("step")
                .and_then(serde_json::Value::as_str)
                .expect("actor queue call has a step label");
            self.started
                .send(step.to_owned())
                .expect("actor queue test is listening for W1 entry");
            if input.get("hold").and_then(serde_json::Value::as_bool) == Some(true) {
                let release = self
                    .releases
                    .lock()
                    .unwrap()
                    .remove(step)
                    .expect("held call has a release gate");
                tokio::select! {
                    _ = release => {},
                    _ = async {
                        if let Some(cancellation) = ctx.cancel.as_ref() {
                            cancellation.cancelled().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {},
                }
            }
            Ok(ToolCallResult::from_data(json!({ "step": step })))
        }
    }

    fn actor_queue_orch(
        held_steps: &[&str],
    ) -> (
        Arc<ConversationOrchestrator>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>,
    ) {
        let (started, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut releases = std::collections::HashMap::new();
        let mut release_senders = std::collections::HashMap::new();
        for step in held_steps {
            let (tx, rx) = tokio::sync::oneshot::channel();
            releases.insert((*step).to_owned(), rx);
            release_senders.insert((*step).to_owned(), tx);
        }
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(ActorQueueTool {
            started,
            releases: Arc::new(Mutex::new(releases)),
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        (orch, started_rx, release_senders)
    }

    async fn add_actor_queue_call(
        executor: &mut StreamingToolExecutor<'_>,
        step: &str,
        concurrency_safe: bool,
        hold: bool,
        assistant_id: MessageId,
    ) {
        executor
            .add_tool(
                ToolUseId::new(),
                "ActorQueue".into(),
                json!({ "step": step, "safe": concurrency_safe, "hold": hold }),
                None,
                assistant_id,
            )
            .await;
    }

    struct StrictSchemaSafeTool;

    #[async_trait]
    impl Tool for StrictSchemaSafeTool {
        fn name(&self) -> &str {
            "StrictSchemaSafeTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| {
                    json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    })
                });
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
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
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            "strict-schema-safe-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    /// Build an orchestrator whose registry contains a single `SafeTool`.
    fn orch_with_safe_tool() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    struct FixedPostToolContextHook;

    #[async_trait]
    impl hooks::executor::BuiltinHookHandler for FixedPostToolContextHook {
        async fn handle(
            &self,
            _event: &hooks::events::HookEvent,
            _ctx: &hooks::registry::HookContext,
        ) -> hooks::HookResult {
            hooks::HookResult {
                outcome: hooks::HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                response: Some(hooks::response::HookResponse {
                    additional_context: Some("POST-ERROR-CTX".into()),
                    ..hooks::response::HookResponse::default()
                }),
            }
        }

        fn id(&self) -> &str {
            "post-tool-context"
        }
    }

    struct UnusedHookHttp;

    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHookHttp {
        async fn request(
            &self,
            _request: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused test hook HTTP transport".into(),
            ))
        }

        async fn stream_sse(
            &self,
            _request: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused test hook HTTP transport".into(),
            ))
        }
    }

    struct UnusedHookRuntime;

    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedHookRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal(
                "unused test hook runtime".into(),
            ))
        }

        async fn sleep(&self, _duration: std::time::Duration) {}

        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    fn post_tool_context_hook_executor() -> Arc<hooks::executor::HookExecutorImpl> {
        let mut registry = hooks::HookRegistry::new();
        registry.register(hooks::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "post-tool-context".into(),
            events: vec![hooks::HookEventType::PostToolUse],
            if_condition: None,
            executor: hooks::definition::HookExecutor::Builtin {
                handler_id: "post-tool-context".into(),
            },
            source: hooks::definition::HookSource::Session,
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
        let mut executor = hooks::executor::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(UnusedHookHttp),
            Arc::new(UnusedHookRuntime),
        );
        executor.register_builtin(Arc::new(FixedPostToolContextHook));
        Arc::new(executor)
    }

    struct GatedHookMcp {
        started: Arc<Notify>,
        release: Arc<Notify>,
        finished: Arc<Notify>,
    }

    #[async_trait]
    impl hooks::mcp_invoker::HookMcpInvoker for GatedHookMcp {
        async fn invoke(
            &self,
            _request: hooks::mcp_invoker::HookMcpInvocation,
        ) -> hooks::mcp_invoker::HookMcpInvocationResult {
            self.started.notify_one();
            self.release.notified().await;
            self.finished.notify_one();
            hooks::mcp_invoker::HookMcpInvocationResult::Success {
                text_content: vec![
                    r#"{"systemMessage":"ASYNC-SYSTEM","hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"ASYNC-CONTEXT"}}"#
                        .into(),
                ],
            }
        }
    }

    struct TokioHookRuntime {
        next_id: std::sync::atomic::AtomicU64,
        async_hook_finished: Arc<Notify>,
        handles: std::sync::Mutex<std::collections::HashMap<u64, tokio::task::JoinHandle<()>>>,
    }

    impl TokioHookRuntime {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                next_id: std::sync::atomic::AtomicU64::new(1),
                async_hook_finished: Arc::new(Notify::new()),
                handles: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        }
    }

    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for TokioHookRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let finished = Arc::clone(&self.async_hook_finished);
            let is_async_hook = name == "async_hook";
            let handle = tokio::spawn(async move {
                task.await;
                if is_async_hook {
                    finished.notify_one();
                }
            });
            self.handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id, handle);
            Ok(lingxi_core::host::BackgroundTaskHandle {
                task_name: name.into(),
                task_id: id,
            })
        }

        async fn sleep(&self, duration: std::time::Duration) {
            tokio::time::sleep(duration).await;
        }

        async fn cancel(
            &self,
            handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            if let Some(task) = self
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&handle.task_id)
            {
                task.abort();
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct TestAsyncHookResponses(
        std::sync::Mutex<Vec<crate::prompt::async_hook_response::AsyncHookResponse>>,
    );

    impl TestAsyncHookResponses {
        fn push(&self, response: crate::prompt::async_hook_response::AsyncHookResponse) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(response);
        }
    }

    #[async_trait]
    impl crate::prompt::async_hook_response::AsyncHookResponseProvider for TestAsyncHookResponses {
        async fn take_pending_responses(&self) -> Vec<hooks::ExactHookText> {
            std::mem::take(
                &mut *self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
            .into_iter()
            .map(|response| response.text)
            .collect()
        }

        async fn take_pending_with_events(
            &self,
        ) -> Vec<crate::prompt::async_hook_response::AsyncHookResponse> {
            std::mem::take(
                &mut *self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
    }

    fn delayed_async_post_tool_hook() -> (
        Arc<hooks::executor::HookExecutorImpl>,
        tokio::sync::mpsc::Receiver<hooks::async_registry::HookCompletionEnvelope>,
        Arc<Notify>,
        Arc<Notify>,
        Arc<Notify>,
        Arc<TokioHookRuntime>,
    ) {
        let runtime = TokioHookRuntime::new();
        let runtime_spawner: Arc<dyn lingxi_core::host::RuntimeSpawner> = runtime.clone();
        let (completion_tx, completion_rx) =
            tokio::sync::mpsc::channel::<hooks::async_registry::HookCompletionEnvelope>(4);
        let async_registry = Arc::new(hooks::AsyncHookRegistry::new(
            Arc::clone(&runtime_spawner),
            completion_tx,
        ));
        let mut registry = hooks::HookRegistry::new();
        registry.register(hooks::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "delayed-post-tool".into(),
            events: vec![hooks::HookEventType::PostToolUse],
            if_condition: None,
            executor: hooks::definition::HookExecutor::McpTool {
                server: "test".into(),
                tool: "delayed".into(),
                input: std::collections::HashMap::new(),
            },
            source: hooks::definition::HookSource::Session,
            blocking: false,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: true,
            async_timeout: Some(std::time::Duration::from_secs(5)),
            rewake_message: Some("continue after delayed validation".into()),
        });
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let executor = hooks::executor::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(UnusedHookHttp),
            runtime_spawner,
        )
        .with_mcp_invoker(Arc::new(GatedHookMcp {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            finished: Arc::clone(&finished),
        }))
        .with_async_registry(async_registry);
        (
            Arc::new(executor),
            completion_rx,
            started,
            release,
            finished,
            runtime,
        )
    }

    #[tokio::test]
    async fn accepted_async_post_hook_rewakes_after_normal_executor_close() {
        use hooks::attachment::HookPublicationGuard;

        let (
            hook_executor,
            mut completion_rx,
            hook_started,
            hook_release,
            _hook_finished,
            _runtime,
        ) = delayed_async_post_tool_hook();

        let provider = Arc::new(TestAsyncHookResponses::default());
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
            message_start("async-hook-rewake", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "handled delayed validation"),
            content_block_stop(0),
            crate::test_support_stream::message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let streaming_api: Arc<dyn crate::StreamingApiClient> = streaming.clone();
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                streaming_api,
                Arc::new(tools),
                hook_executor,
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_async_hook_responses(provider.clone()),
        );

        // Model the originating turn holding the gate while its W1 executor
        // closes. The async completion is then consumed by the production
        // re-wake entry point after that gate opens.
        let held_turn = orch.turn_gate.lock().await;
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .expect("the W1 scheduler is bound to this orchestrator");
        let dispatch_finished = executor
            .add_tool_observed_dispatch(
                ToolUseId::from("toolu-delayed-post-tool"),
                "SafeTool".into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), hook_started.notified())
            .await
            .expect("W1 fired the real non-blocking PostToolUse hook");
        tokio::time::timeout(std::time::Duration::from_secs(2), dispatch_finished)
            .await
            .expect("W1 dispatch completes while its background hook remains parked")
            .expect("dispatch finish signal");
        assert_eq!(executor.drain_ready_without_queue().await, 1);
        assert_eq!(
            executor.take_newly_completed().len(),
            1,
            "the real ToolResult crosses the actor's accepted Tn boundary"
        );
        executor.finish_context_layers().await.unwrap();
        drop(executor);

        let rewake_orch = Arc::clone(&orch);
        let rewake_provider = Arc::clone(&provider);
        let (rewake_queued_tx, rewake_queued_rx) = tokio::sync::oneshot::channel();
        let rewake = tokio::spawn(async move {
            let envelope =
                tokio::time::timeout(std::time::Duration::from_secs(2), completion_rx.recv())
                    .await
                    .expect("background hook completes after release")
                    .expect("completion channel stays open");
            assert!(envelope.is_current());
            let guard = envelope
                .publication_guard
                .clone()
                .expect("W1 async hook retains its generation guard");
            let response = envelope
                .result
                .response
                .as_ref()
                .expect("the async hook parsed its response");
            let mut lines = Vec::new();
            if let Some(system_message) = response.system_message.as_ref() {
                lines.push(system_message.clone());
            }
            if let Some(additional_context) = response.additional_context.as_ref() {
                lines.push(additional_context.clone());
            }
            rewake_provider.push(crate::prompt::async_hook_response::AsyncHookResponse {
                text: hooks::ExactHookText::join(&lines, "\n"),
                hook_event: envelope.hook_event,
                publication_guard: Some(Arc::clone(&guard)),
            });
            let cancellation = guard
                .generation_cancellation_token()
                .expect("W1 guard exposes its generation cancellation token");
            let _ = rewake_queued_tx.send(());
            rewake_orch.run_async_hook_rewake(Some(cancellation)).await
        });

        hook_release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), rewake_queued_rx)
            .await
            .expect("completion schedules the re-wake while the original turn gate is held")
            .expect("rewake queued signal");
        drop(held_turn);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), rewake)
            .await
            .expect("normal-close re-wake obtains the turn gate")
            .expect("rewake task joins")
            .expect("rewake turn succeeds");
        assert!(matches!(outcome, crate::conversation::TurnOutcome::EndTurn));

        let calls = streaming.captured_calls().await;
        assert_eq!(
            calls.len(),
            1,
            "one real model request consumes the hook result"
        );
        let request_text = calls[0]
            .messages
            .iter()
            .map(ConversationMessage::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(request_text.contains("ASYNC-SYSTEM"), "{request_text}");
        assert!(request_text.contains("ASYNC-CONTEXT"), "{request_text}");
        assert!(
            request_text.contains("continue after delayed validation"),
            "{request_text}"
        );
    }

    #[tokio::test]
    async fn clear_rejects_late_async_post_hook_completion_from_normally_closed_executor() {
        let (hook_executor, mut completion_rx, hook_started, hook_release, hook_finished, runtime) =
            delayed_async_post_tool_hook();
        let provider = Arc::new(TestAsyncHookResponses::default());
        let streaming = Arc::new(MockStreamingApiClient::empty());
        let streaming_api: Arc<dyn crate::StreamingApiClient> = streaming.clone();
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                streaming_api,
                Arc::new(tools),
                hook_executor,
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_async_hook_responses(provider.clone()),
        );
        let original_session = orch.session.lock().await.session_id;
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .expect("the W1 scheduler is bound to this orchestrator");
        let dispatch_finished = executor
            .add_tool_observed_dispatch(
                ToolUseId::from("toolu-cleared-delayed-post-tool"),
                "SafeTool".into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), hook_started.notified())
            .await
            .expect("W1 fired the real non-blocking PostToolUse hook");
        tokio::time::timeout(std::time::Duration::from_secs(2), dispatch_finished)
            .await
            .expect("W1 dispatch completes with the async hook parked")
            .expect("dispatch finish signal");
        assert_eq!(executor.drain_ready_without_queue().await, 1);
        assert_eq!(executor.take_newly_completed().len(), 1);
        executor.finish_context_layers().await.unwrap();
        drop(executor);

        lingxi_core::host::OrchestratorHandle::clear_session(orch.as_ref())
            .await
            .expect("clear rotates the session hook owner");
        assert_ne!(orch.session.lock().await.session_id, original_session);
        hook_release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), hook_finished.notified())
            .await
            .expect("the real asynchronous MCP hook is released");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            runtime.async_hook_finished.notified(),
        )
        .await
        .expect("the async registry finishes its rejected publication path");

        assert!(completion_rx.try_recv().is_err());
        assert!(
            provider
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        assert!(streaming.captured_calls().await.is_empty());
    }

    #[tokio::test]
    async fn provider_error_keeps_accepted_w1_post_tool_attachment_before_reset() {
        let dir = tempfile::tempdir().expect("temporary session directory");
        let path = dir.path().join("session.jsonl");
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
        );
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tools),
                post_tool_context_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_jsonl_writer(writer),
        );
        orch.set_tool_frame_buffering(true).await;
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .expect("the W1 scheduler is bound to this orchestrator");
        let id = ToolUseId::from("toolu-provider-error-after-post-hook");
        let dispatch_finished = executor
            .add_tool_observed_dispatch(
                id.clone(),
                "SafeTool".into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), dispatch_finished)
            .await
            .expect("real W1 dispatch and PostToolUse hook finish before provider error")
            .expect("dispatch finish signal");
        assert!(
            orch.transcript
                .pending_hook_attachments
                .lock()
                .await
                .contains_key(id.as_str())
        );

        // This is the provider-error finalizer used by the streaming driver.
        // It must commit Tn-accepted results and their attachments before it
        // resets the tool actor generation and rejects unmatched work.
        let mut partial = crate::streaming_loop::PumpedTurn::default();
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        orch.settle_stream_error_attempt(
            &mut executor,
            &mut partial,
            &mut settlement,
            MessageId::new(),
            "provider disconnected after completed tool",
        )
        .await;

        let raw = std::fs::read_to_string(path).expect("accepted result and attachment persisted");
        let lines: Vec<&str> = raw.lines().filter(|line| !line.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "tool result then one PostToolUse attachment"
        );
        let result: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let attachment: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(result["type"], "user");
        assert!(lines[0].contains(id.as_str()), "{result}");
        assert_eq!(attachment["attachment"]["type"], "hook_additional_context");
        assert_eq!(attachment["attachment"]["content"][0], "POST-ERROR-CTX");
        assert!(
            orch.transcript
                .pending_hook_attachments
                .lock()
                .await
                .get(id.as_str())
                .is_none()
        );
    }

    fn orch_with_safe_and_strict_schema_tool() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(StrictSchemaSafeTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    #[tokio::test]
    async fn add_unknown_tool_completes_immediately_with_wrapper() {
        let orch = ConversationOrchestrator::into_shared(orch_empty());
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "Nope".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        let t = &exec.tools[0];
        assert_eq!(t.status, ToolStatus::Completed);
        assert!(t.is_concurrency_safe);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = t.result.as_ref().unwrap()
        else {
            panic!("expected ToolResult block")
        };
        assert!(is_error.unwrap_or(false));
        assert_eq!(
            content,
            "<tool_use_error>Error: No such tool available: Nope</tool_use_error>"
        );
    }

    #[test]
    fn unknown_tool_suffix_is_empty_for_genuinely_unknown() {
        let tools = ToolRegistry::new();
        assert_eq!(unknown_tool_suffix("Nope", &tools, false, false), "");
    }

    #[test]
    fn unknown_tool_suffix_points_glob_and_grep_at_bash() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Bash")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("Glob", &tools, false, false),
            ". Glob is not available in this session \u{2014} find files with `find` via the Bash tool instead."
        );
        assert_eq!(
            unknown_tool_suffix("Grep", &tools, false, false),
            ". Grep is not available in this session \u{2014} search file contents with `grep` via the Bash tool instead."
        );
    }

    #[test]
    fn unknown_tool_suffix_glob_without_shell_is_disabled() {
        let tools = ToolRegistry::new();
        assert_eq!(
            unknown_tool_suffix("Glob", &tools, false, false),
            ". Glob is disabled for this session."
        );
    }

    #[test]
    fn unknown_tool_suffix_names_disconnected_mcp_server() {
        let tools = ToolRegistry::new();
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, false, false),
            ". Its MCP server 'github' has disconnected. Continue without this tool; it becomes callable again only if the server reconnects."
        );
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, true, false),
            ". Its MCP server 'github' is not available in this context. Continue without this tool."
        );
    }

    #[test]
    fn unknown_tool_suffix_subagent_restricted_catalog() {
        let tools = ToolRegistry::new();
        assert_eq!(
            unknown_tool_suffix("EnterPlanMode", &tools, true, false),
            ". EnterPlanMode is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        );
        assert_eq!(
            unknown_tool_suffix("WaitForMcpServers", &tools, true, false),
            ". WaitForMcpServers is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        );
        assert_eq!(
            unknown_tool_suffix("EnterPlanMode", &tools, false, false),
            ""
        );
    }

    #[test]
    fn unknown_tool_suffix_pending_mcp_points_at_wait_for_mcp_servers() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("WaitForMcpServers")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, false, false),
            ". The MCP server 'github' is still connecting. Call WaitForMcpServers to wait for it, then try again."
        );
        // Subagent skips `l5o` (`r?"":l5o`) and uses the disconnected arm.
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, true, false),
            ". Its MCP server 'github' is not available in this context. Continue without this tool."
        );
    }

    #[test]
    fn unknown_tool_suffix_coordinator_points_at_agent() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Agent")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed::new("Read")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("Read", &tools, false, true),
            ". Read is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
        assert_eq!(
            unknown_tool_suffix("Read", &tools, false, false),
            ". Read is disabled for this session, in subagents as well as here."
        );
        tools.register_builtin(Arc::new(SafeToolNamed::new("Skill")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("Skill", &tools, false, true),
            ". Skill is disabled for this session, in subagents as well as here."
        );
    }

    #[test]
    fn unknown_tool_suffix_catalog_disabled_hides_glob_via_shell() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Bash")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed::new("Glob")) as Arc<dyn Tool>);
        tools.set_session_tool_allowlist(&["Bash".to_string()]);
        assert_eq!(
            unknown_tool_suffix("Glob", &tools, false, false),
            ". Glob is disabled for this session, in subagents as well as here."
        );
    }

    #[test]
    fn unknown_tool_suffix_webfetch_in_catalog_is_disabled_not_artifact() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("WebFetch")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed::new("Agent")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("WebFetch", &tools, false, false),
            ". WebFetch is disabled for this session, in subagents as well as here."
        );
        assert_eq!(
            unknown_tool_suffix("WebFetch", &tools, false, true),
            ". WebFetch is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
    }

    #[test]
    fn unknown_tool_suffix_qbt_matches_canonical_alias() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Agent")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed {
            name: "ListAgents",
            aliases: &["ListPeers"],
            ..SafeToolNamed::new("ListAgents")
        }) as Arc<dyn Tool>);
        tools.set_session_tool_allowlist(&["Agent".to_string()]);
        assert_eq!(
            unknown_tool_suffix("ListPeers", &tools, false, true),
            ". ListPeers is disabled for this session, in subagents as well as here."
        );
    }

    struct StubCoordinatorMode {
        enabled: bool,
    }

    impl lingxi_core::host::coordinator_mode::CoordinatorModeHandle for StubCoordinatorMode {
        fn is_enabled(&self) -> bool {
            self.enabled
        }
    }

    fn orch_with_named_tools(names: &[&'static str]) -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        for name in names {
            registry.register_builtin(Arc::new(SafeToolNamed::new(name)) as Arc<dyn Tool>);
        }
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_coordinator_simple_mode_for_test(false)
        .with_coordinator_pool_for_test(false, &[])
    }

    #[test]
    fn unknown_tool_suffix_for_uses_wired_coordinator_mode() {
        let orch = ConversationOrchestrator::into_shared(
            orch_with_named_tools(&["Agent", "Read"])
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true })),
        );
        assert_eq!(
            unknown_tool_suffix_for("Read", &orch),
            ". Read is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
    }

    #[tokio::test]
    async fn observing_completed_actor_keeps_its_dispatch_payload_for_tn() {
        let orch = ConversationOrchestrator::into_shared(orch_with_safe_tool());
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let id = ToolUseId::new();
        exec.add_tool(
            id.clone(),
            "SafeTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exec.scheduler.wait_for_progress(),
        )
        .await
        .expect("the tool finishes before the status-only observation")
        .unwrap();
        exec.sync_statuses().await;
        assert!(
            exec.tools[0].result.is_none(),
            "the status read does not transfer a result"
        );
        assert_eq!(exec.drain_ready().await, 1);
        let results = exec.take_newly_completed();
        assert_eq!(results.len(), 1);
        assert!(matches!(
            &results[0].block,
            ContentBlock::ToolResult { tool_use_id, is_error, .. }
                if tool_use_id == &id && !is_error.unwrap_or(false)
        ));
        assert!(
            exec.take_newly_completed().is_empty(),
            "the outcome is yielded once"
        );
    }

    #[tokio::test]
    async fn add_tool_disabled_coordinator_handle_dispatches_worker_tool() {
        let orch = ConversationOrchestrator::into_shared(
            orch_with_named_tools(&["Agent", "Read"])
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: false })),
        );
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "Read".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        assert_eq!(exec.tools[0].resolved_tool.as_ref().unwrap().name(), "Read");
        let results = exec.run_to_completion().await.unwrap();
        assert_eq!(results.len(), exec.tools.len());
        assert!(
            results.iter().all(|block| matches!(block,
                ContentBlock::ToolResult { is_error, .. } if !is_error.unwrap_or(false)
            )),
            "every available tool completes successfully through the owned actor"
        );
    }

    #[tokio::test]
    async fn add_tool_simple_mode_keeps_pool_but_suppresses_worker_redirect() {
        let orch = ConversationOrchestrator::into_shared(
            orch_with_named_tools(&["Agent", "Read"])
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }))
                .with_coordinator_simple_mode_for_test(true),
        );
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "Read".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        assert_eq!(exec.tools[0].status, ToolStatus::Completed);
        let ContentBlock::ToolResult { content, .. } = exec.tools[0].result.as_ref().unwrap()
        else {
            panic!("expected error")
        };
        assert!(content.contains("disabled for this session"));
        assert!(!content.contains("run it from a worker"));
    }

    #[tokio::test]
    async fn add_tool_coordinator_hidden_worker_tool_gets_y7e() {
        let orch = ConversationOrchestrator::into_shared(
            orch_with_named_tools(&["Agent", "Read"])
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true })),
        );
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "Read".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        let t = &exec.tools[0];
        assert_eq!(t.status, ToolStatus::Completed);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = t.result.as_ref().unwrap()
        else {
            panic!("expected ToolResult block")
        };
        assert!(is_error.unwrap_or(false));
        assert_eq!(
            content,
            "<tool_use_error>Error: No such tool available: Read. Read is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead.</tool_use_error>"
        );
    }

    #[tokio::test]
    async fn add_tool_coordinator_pool_dispatches_user_and_plan_controls() {
        let names = [
            "Agent",
            "SendMessage",
            "TaskStop",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "subscribe_pr_activity",
            "unsubscribe_pr_activity",
        ];
        let orch = ConversationOrchestrator::into_shared(
            orch_with_named_tools(&names)
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }))
                .with_coordinator_pool_for_test(
                    false,
                    &["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"],
                ),
        );
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        for name in names {
            exec.add_tool(
                ToolUseId::new(),
                name.into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
            assert_eq!(
                exec.tools
                    .last()
                    .unwrap()
                    .resolved_tool
                    .as_ref()
                    .unwrap()
                    .name(),
                name
            );
        }
        let results = exec.run_to_completion().await.unwrap();
        assert_eq!(results.len(), exec.tools.len());
        assert!(
            results.iter().all(|block| matches!(block,
                ContentBlock::ToolResult { is_error, .. } if !is_error.unwrap_or(false)
            )),
            "every available tool completes successfully through the owned actor"
        );
    }

    #[tokio::test]
    async fn add_tool_coordinator_qbt_tools_remain_available() {
        let names = [
            "Skill",
            "ListAgents",
            "Workflow",
            "ReadNotifications",
            "StructuredOutput",
        ];
        let orch = ConversationOrchestrator::into_shared(
            orch_with_named_tools(&names)
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true })),
        );
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        for name in names {
            exec.add_tool(
                ToolUseId::new(),
                name.into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
            assert_eq!(
                exec.tools
                    .last()
                    .unwrap()
                    .resolved_tool
                    .as_ref()
                    .unwrap()
                    .name(),
                name
            );
        }
        let results = exec.run_to_completion().await.unwrap();
        assert_eq!(results.len(), exec.tools.len());
        assert!(
            results.iter().all(|block| matches!(block,
                ContentBlock::ToolResult { is_error, .. } if !is_error.unwrap_or(false)
            )),
            "every available tool completes successfully through the owned actor"
        );
    }

    #[test]
    fn coordinator_pool_metadata_and_extras_follow_idr() {
        let orch = orch_with_named_tools(&[]);
        assert!(
            orch.is_coordinator_pool_tool(&SafeToolNamed::new(
                "mcp__github__subscribe_pr_activity"
            ))
        );
        let comms = SafeToolNamed {
            role: Some("comms"),
            ..SafeToolNamed::new("mcp__chat__send")
        };
        assert!(orch.is_coordinator_pool_tool(&comms));
        for name in [
            "AskUserQuestion",
            "ExitPlanMode",
            "SendUserMessage",
            "SendUserFile",
            "Bash",
        ] {
            assert!(
                !orch.is_coordinator_pool_tool(&SafeToolNamed::new(name)),
                "{name}"
            );
        }
        let orch = orch.with_coordinator_pool_for_test(true, &["AskUserQuestion", "V1", "Parent"]);
        for name in ["AskUserQuestion", "SendUserMessage", "SendUserFile"] {
            assert!(
                orch.is_coordinator_pool_tool(&SafeToolNamed::new(name)),
                "{name}"
            );
        }
        assert!(orch.is_coordinator_pool_tool(&SafeToolNamed {
            v1: Some("V1"),
            ..SafeToolNamed::new("split")
        }));
        assert!(orch.is_coordinator_pool_tool(&SafeToolNamed {
            parent: Some("Parent"),
            ..SafeToolNamed::new("split")
        }));
        assert!(!orch.is_coordinator_pool_tool(&SafeToolNamed {
            aliases: &["V1"],
            ..SafeToolNamed::new("split")
        }));
    }

    #[test]
    fn coordinator_redirect_requires_enabled_worker_and_agent_tools() {
        let mut orch =
            orch_with_named_tools(&["Agent", "Read", "AskUserQuestion", "ExitPlanMode", "Custom"])
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }));
        for name in ["AskUserQuestion", "ExitPlanMode", "Custom"] {
            assert!(unknown_tool_suffix_for(name, &orch).contains("disabled for this session"));
        }
        for denied in ["Read", "Agent"] {
            *orch.tool_pool_denied_names.write().unwrap() = vec![denied.into()];
            assert!(unknown_tool_suffix_for("Read", &orch).contains("disabled for this session"));
        }
        orch.tool_pool_denied_names.write().unwrap().clear();
        *orch.main_agent_tool_names.write().unwrap() = Some(["Read".into()].into_iter().collect());
        assert!(unknown_tool_suffix_for("Read", &orch).contains("disabled for this session"));
        *orch.main_agent_tool_names.write().unwrap() = None;
        assert!(unknown_tool_suffix_for("Read", &orch).contains("run it from a worker"));
        Arc::get_mut(&mut orch.tools)
            .unwrap()
            .set_session_tool_allowlist(&["Agent".into()]);
        assert!(unknown_tool_suffix_for("Read", &orch).contains("disabled for this session"));
    }

    #[test]
    fn disabled_brief_uses_canonical_message_fallback() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed {
            aliases: &["Brief"],
            ..SafeToolNamed::new("SendUserMessage")
        }));
        assert_eq!(
            unknown_tool_suffix("Brief", &tools, false, true),
            ". Brief is not enabled in this session \u{2014} write your message as normal assistant text instead."
        );
    }

    struct SafeToolNamed {
        name: &'static str,
        aliases: &'static [&'static str],
        role: Option<&'static str>,
        v1: Option<&'static str>,
        parent: Option<&'static str>,
    }

    impl SafeToolNamed {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                aliases: &[],
                role: None,
                v1: None,
                parent: None,
            }
        }
    }

    #[async_trait]
    impl Tool for SafeToolNamed {
        fn name(&self) -> &str {
            self.name
        }
        fn aliases(&self) -> &[&str] {
            self.aliases
        }
        fn mcp_role(&self) -> Option<&str> {
            self.role
        }
        fn underlying_v1_tool_name(&self) -> Option<&str> {
            self.v1
        }
        fn family_parent_tool_name(&self) -> Option<&str> {
            self.parent
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
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
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            self.name.into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    #[tokio::test]
    async fn add_known_concurrency_safe_tool_starts_at_admission() {
        let orch = ConversationOrchestrator::into_shared(orch_with_safe_tool());
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "SafeTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        let t = &exec.tools[0];
        assert_eq!(t.status, ToolStatus::Executing);
        assert!(t.is_concurrency_safe);
        assert!(t.result.is_none());
    }

    #[tokio::test]
    async fn malformed_input_is_concurrency_unsafe() {
        let orch = ConversationOrchestrator::into_shared(orch_with_safe_and_strict_schema_tool());
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "StrictSchemaSafeTool".into(),
            json!({}),
            None,
            a,
        )
        .await;

        assert!(!exec.tools[0].is_concurrency_safe);
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
    }

    // ============================================================================
    // Task 7: concurrency-safety and autonomous-scheduling tests
    // ============================================================================

    use super::can_execute;

    #[test]
    fn can_execute_respects_concurrency_safety() {
        assert!(can_execute(&[], true));
        assert!(can_execute(&[], false)); // nothing running → ok
        assert!(can_execute(&[true, true], true)); // all safe + candidate safe → ok
        assert!(!can_execute(&[true], false)); // candidate unsafe, something running → no
        assert!(!can_execute(&[false], true)); // an unsafe tool running → no
        assert!(!can_execute(&[true, true], false)); // many safe running, unsafe candidate → no
    }

    /// A minimal concurrency-UNSAFE tool for ordering/barrier tests.
    struct UnsafeTool;

    #[async_trait]
    impl Tool for UnsafeTool {
        fn name(&self) -> &str {
            "UnsafeTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            false
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            false
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            "unsafe-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    struct StreamBoundarySerialTool {
        release_first: Arc<Notify>,
        second_started: Arc<Notify>,
        release_second: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for StreamBoundarySerialTool {
        fn name(&self) -> &str {
            "StreamBoundarySerial"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| {
                    json!({
                        "type": "object",
                        "properties": { "step": { "type": "string" } },
                        "required": ["step"]
                    })
                });
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            false
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            false
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            "stream boundary serial tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            match input.get("step").and_then(serde_json::Value::as_str) {
                Some("first") => self.release_first.notified().await,
                Some("second") => {
                    self.second_started.notify_one();
                    self.release_second.notified().await;
                }
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "unexpected step: {other:?}"
                    )));
                }
            }
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: json!({ "content": input["step"] }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    struct ReleaseFirstOnSecondAssistantRow {
        release_first: Arc<Notify>,
        release_second: Arc<Notify>,
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl lingxi_core::host::OutputStream for ReleaseFirstOnSecondAssistantRow {
        async fn emit_assistant_block_identity(&self, block_key: u64, _row_id: &MessageId) {
            self.events
                .lock()
                .unwrap()
                .push(format!("assistant:{block_key}"));
            if block_key == 1 {
                self.release_first.notify_one();
            } else if block_key == 2 {
                self.release_second.notify_one();
            }
        }
        async fn emit_text(&self, _text: &str, _utf16_code_units: Option<&[u16]>) {}
        async fn emit_tool_call(&self, _id: &ToolUseId, _tool: &str, _input: &serde_json::Value, _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>) {}
        async fn emit_tool_result(
            &self,
            id: &ToolUseId,
            _tool: &str,
            _model_text: &str,
            _result: &serde_json::Value,
         _projection: Option<&lingxi_core::host::ToolResultProjection>) {
            self.events.lock().unwrap().push(format!("result:{id}"));
        }
        async fn emit_end_turn(
            &self,
            _stop_reason: &str,
            _cost: &lingxi_core::host::orchestrator::CostSnapshot,
        ) {
        }
    }

    /// Build an orchestrator with both SafeTool and UnsafeTool.
    fn orch_with_both_tools() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(UnsafeTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    async fn next_actor_start(
        started: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(2), started.recv())
            .await
            .expect("eligible actor call reaches Tool::call")
            .expect("actor start channel remains open")
    }

    async fn assert_no_actor_start(started: &mut tokio::sync::mpsc::UnboundedReceiver<String>) {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), started.recv())
                .await
                .is_err(),
            "a queued call must not enter Tool::call across an active barrier"
        );
    }

    async fn release_actor_call(
        releases: &mut std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>,
        step: &str,
    ) {
        releases
            .remove(step)
            .expect("held actor call has a release sender")
            .send(())
            .expect("held actor call is still waiting");
    }

    /// Safe A admits first. Unsafe B is an ordering barrier, so safe C cannot
    /// enter W1 until B has completed. Each assertion observes the real
    /// `Tool::call` seam; no test-only queue pump is involved.
    #[tokio::test]
    async fn actor_starts_safe_prefix_then_respects_unsafe_barrier() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["A", "B"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", true, true, assistant).await;
        assert_eq!(next_actor_start(&mut started).await, "A");
        add_actor_queue_call(&mut exec, "B", false, true, assistant).await;
        add_actor_queue_call(&mut exec, "C", true, false, assistant).await;
        exec.sync_statuses().await;
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_no_actor_start(&mut started).await;

        release_actor_call(&mut releases, "A").await;
        assert_eq!(next_actor_start(&mut started).await, "B");
        exec.sync_statuses().await;
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_no_actor_start(&mut started).await;

        release_actor_call(&mut releases, "B").await;
        assert_eq!(next_actor_start(&mut started).await, "C");
        exec.run_to_completion().await.unwrap();
    }

    /// All leading concurrency-safe calls enter W1 without waiting for provider
    /// or Tn polling, and hold their slots until explicitly released.
    #[tokio::test]
    async fn actor_starts_safe_calls_concurrently() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["A", "B"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", true, true, assistant).await;
        add_actor_queue_call(&mut exec, "B", true, true, assistant).await;
        let mut entries = vec![
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
        ];
        entries.sort();
        assert_eq!(entries, vec!["A", "B"]);
        exec.sync_statuses().await;
        assert!(
            exec.tools
                .iter()
                .all(|tool| tool.status == ToolStatus::Executing)
        );
        release_actor_call(&mut releases, "A").await;
        release_actor_call(&mut releases, "B").await;
        exec.run_to_completion().await.unwrap();
    }

    /// An executing unsafe call blocks later safe calls until it finishes.
    #[tokio::test]
    async fn actor_unsafe_call_blocks_following_safe_call() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["A"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", false, true, assistant).await;
        assert_eq!(next_actor_start(&mut started).await, "A");
        add_actor_queue_call(&mut exec, "B", true, false, assistant).await;
        exec.sync_statuses().await;
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
        assert_no_actor_start(&mut started).await;

        release_actor_call(&mut releases, "A").await;
        assert_eq!(next_actor_start(&mut started).await, "B");
        exec.run_to_completion().await.unwrap();
    }

    /// Discard cancels the running call and tombstones a queued suffix. Even
    /// after A cooperatively exits, B never reaches the real Tool::call seam.
    #[tokio::test]
    async fn discard_prevents_queued_tool_from_entering_w1() {
        let (orch, mut started, _releases) = actor_queue_orch(&["A"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", false, true, assistant).await;
        assert_eq!(next_actor_start(&mut started).await, "A");
        add_actor_queue_call(&mut exec, "B", true, false, assistant).await;
        exec.sync_statuses().await;
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
        assert_no_actor_start(&mut started).await;

        exec.discard().await;
        assert!(exec.tools.is_empty());
        assert_no_actor_start(&mut started).await;
    }

    /// A safe prefix may run together. The unsafe call and its safe suffix are
    /// held until both prefix calls settle; the suffix then follows the unsafe
    /// call rather than jumping the barrier.
    #[tokio::test]
    async fn actor_safe_prefix_and_unsafe_suffix_keep_registration_order() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["A", "B", "C"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", true, true, assistant).await;
        add_actor_queue_call(&mut exec, "B", true, true, assistant).await;
        let mut entries = vec![
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
        ];
        entries.sort();
        assert_eq!(entries, vec!["A", "B"]);
        add_actor_queue_call(&mut exec, "C", false, true, assistant).await;
        add_actor_queue_call(&mut exec, "D", true, false, assistant).await;
        exec.sync_statuses().await;
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_eq!(exec.tools[3].status, ToolStatus::Queued);
        release_actor_call(&mut releases, "A").await;
        assert_no_actor_start(&mut started).await;
        release_actor_call(&mut releases, "B").await;
        assert_eq!(next_actor_start(&mut started).await, "C");
        exec.sync_statuses().await;
        assert_eq!(exec.tools[3].status, ToolStatus::Queued);
        assert_no_actor_start(&mut started).await;
        release_actor_call(&mut releases, "C").await;
        assert_eq!(next_actor_start(&mut started).await, "D");
        exec.run_to_completion().await.unwrap();
    }

    /// Once the unsafe call settles, the scheduler starts every safe call in
    /// the newly eligible prefix without an executor-side poll.
    #[tokio::test]
    async fn actor_promotes_safe_calls_after_unsafe_call_finishes() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["A", "B", "C"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", false, true, assistant).await;
        assert_eq!(next_actor_start(&mut started).await, "A");
        add_actor_queue_call(&mut exec, "B", true, true, assistant).await;
        add_actor_queue_call(&mut exec, "C", true, true, assistant).await;
        exec.sync_statuses().await;
        assert!(
            exec.tools[1..]
                .iter()
                .all(|tool| tool.status == ToolStatus::Queued)
        );
        release_actor_call(&mut releases, "A").await;
        let mut entries = vec![
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
        ];
        entries.sort();
        assert_eq!(entries, vec!["B", "C"]);
        release_actor_call(&mut releases, "B").await;
        release_actor_call(&mut releases, "C").await;
        exec.run_to_completion().await.unwrap();
    }

    // ============================================================================
    // B6: concurrency cap (binary r1p / i1p) tests
    // ============================================================================

    use lingxi_core::host::tool_use_lifecycle::{
        DEFAULT_MAX_TOOL_USE_CONCURRENCY, max_tool_use_concurrency, max_tool_use_concurrency_from,
    };

    /// Binary `r1p`: `parseInt(env, 10) > 0 ? env : 10`.
    #[test]
    fn max_tool_use_concurrency_parse_matches_binary_r1p() {
        // Absent / empty / non-numeric → default 10.
        assert_eq!(max_tool_use_concurrency_from(None), 10);
        assert_eq!(max_tool_use_concurrency_from(Some("")), 10);
        assert_eq!(max_tool_use_concurrency_from(Some("abc")), 10);
        // Zero and negative → default (binary uses `e > 0`).
        assert_eq!(max_tool_use_concurrency_from(Some("0")), 10);
        assert_eq!(max_tool_use_concurrency_from(Some("-3")), 10);
        // Positive → that value.
        assert_eq!(max_tool_use_concurrency_from(Some("1")), 1);
        assert_eq!(max_tool_use_concurrency_from(Some("5")), 5);
        assert_eq!(max_tool_use_concurrency_from(Some("25")), 25);
        // parseInt leading-prefix semantics: "5x" → 5, "  7 " → 7.
        assert_eq!(max_tool_use_concurrency_from(Some("5x")), 5);
        assert_eq!(max_tool_use_concurrency_from(Some("  7 ")), 7);
        // Default constant is 10.
        assert_eq!(DEFAULT_MAX_TOOL_USE_CONCURRENCY, 10);
    }

    /// With 30 safe calls, the owned scheduler starts only the default ten-call
    /// window. Gates hold those real Tool::call futures while the rest remain
    /// queued; releasing the window lets the autonomous pump finish the suffix.
    #[tokio::test]
    async fn actor_caps_safe_calls_at_default_ten() {
        let _guard = tool_concurrency_env_guard();
        // Ensure no env override leaks in from the environment.
        std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
        assert_eq!(max_tool_use_concurrency(), DEFAULT_MAX_TOOL_USE_CONCURRENCY);

        let held = (0..DEFAULT_MAX_TOOL_USE_CONCURRENCY)
            .map(|index| format!("step-{index}"))
            .collect::<Vec<_>>();
        let held_refs = held.iter().map(String::as_str).collect::<Vec<_>>();
        let (orch, mut started, mut releases) = actor_queue_orch(&held_refs);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        for index in 0..30 {
            let step = format!("step-{index}");
            add_actor_queue_call(
                &mut exec,
                &step,
                true,
                index < DEFAULT_MAX_TOOL_USE_CONCURRENCY,
                assistant,
            )
            .await;
        }
        let mut entries = Vec::with_capacity(DEFAULT_MAX_TOOL_USE_CONCURRENCY);
        for _ in 0..DEFAULT_MAX_TOOL_USE_CONCURRENCY {
            entries.push(next_actor_start(&mut started).await);
        }
        entries.sort();
        assert_eq!(
            entries, held,
            "the first ten registered calls occupy the initial window"
        );
        assert_no_actor_start(&mut started).await;

        let executing = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Executing)
            .count();
        let queued = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Queued)
            .count();
        assert_eq!(
            executing, DEFAULT_MAX_TOOL_USE_CONCURRENCY,
            "no more than {DEFAULT_MAX_TOOL_USE_CONCURRENCY} safe tools may run at once"
        );
        assert_eq!(
            queued,
            30 - DEFAULT_MAX_TOOL_USE_CONCURRENCY,
            "the rest stay Queued"
        );
        // The first N (in received order) are the ones started.
        for i in 0..DEFAULT_MAX_TOOL_USE_CONCURRENCY {
            assert_eq!(
                exec.tools[i].status,
                ToolStatus::Executing,
                "tool {i} should run"
            );
        }
        for i in DEFAULT_MAX_TOOL_USE_CONCURRENCY..30 {
            assert_eq!(
                exec.tools[i].status,
                ToolStatus::Queued,
                "tool {i} should wait"
            );
        }
        for step in &held {
            release_actor_call(&mut releases, step).await;
        }
        exec.run_to_completion().await.unwrap();
    }

    /// `LINGXI_MAX_TOOL_USE_CONCURRENCY` overrides the cap. With the env
    /// set to 3 and 10 safe tools queued, exactly 3 start.
    /// (Mutates env → `--test-threads=1`.)
    #[tokio::test]
    async fn actor_respects_env_concurrency_override() {
        let _guard = tool_concurrency_env_guard();
        std::env::set_var("LINGXI_MAX_TOOL_USE_CONCURRENCY", "3");
        // Guard so a panic/assert failure still clears the env for sibling tests.
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
            }
        }
        let _clear = Clear;

        assert_eq!(max_tool_use_concurrency(), 3);

        let held = vec!["step-0", "step-1", "step-2"];
        let (orch, mut started, mut releases) = actor_queue_orch(&held);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        for index in 0..10 {
            let step = format!("step-{index}");
            add_actor_queue_call(&mut exec, &step, true, index < 3, assistant).await;
        }
        let mut entries = vec![
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
        ];
        entries.sort();
        assert_eq!(entries, held);
        assert_no_actor_start(&mut started).await;

        let executing = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Executing)
            .count();
        assert_eq!(executing, 3, "env override caps safe concurrency at 3");
        assert_eq!(
            exec.tools
                .iter()
                .filter(|t| t.status == ToolStatus::Queued)
                .count(),
            7,
            "remaining 7 stay Queued under the override"
        );
        for step in held {
            release_actor_call(&mut releases, step).await;
        }
        exec.run_to_completion().await.unwrap();
    }

    /// Releasing one safe call frees a slot and the actor starts the next
    /// queued call without a Tn/provider poll, preserving the sliding window.
    #[tokio::test]
    async fn actor_starts_next_safe_call_when_slot_frees() {
        let _guard = tool_concurrency_env_guard();
        std::env::set_var("LINGXI_MAX_TOOL_USE_CONCURRENCY", "2");
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
            }
        }
        let _clear = Clear;

        let (orch, mut started, mut releases) = actor_queue_orch(&["A", "B", "C"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        add_actor_queue_call(&mut exec, "A", true, true, assistant).await;
        add_actor_queue_call(&mut exec, "B", true, true, assistant).await;
        add_actor_queue_call(&mut exec, "C", true, true, assistant).await;
        add_actor_queue_call(&mut exec, "D", true, false, assistant).await;
        let mut initial = vec![
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
        ];
        initial.sort();
        assert_eq!(initial, vec!["A", "B"]);
        // Cap=2 → first two run, last two wait.
        exec.sync_statuses().await;
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_eq!(exec.tools[3].status, ToolStatus::Queued);

        // B completes first and frees one slot; C starts automatically. Hold A
        // and C so D remains queued until another slot opens.
        release_actor_call(&mut releases, "B").await;
        assert_eq!(next_actor_start(&mut started).await, "C");
        exec.sync_statuses().await;
        assert_eq!(exec.tools[2].status, ToolStatus::Executing);
        assert_eq!(
            exec.tools[3].status,
            ToolStatus::Queued,
            "still capped at 2 in-flight"
        );
        assert_no_actor_start(&mut started).await;
        release_actor_call(&mut releases, "A").await;
        release_actor_call(&mut releases, "C").await;
        assert_eq!(next_actor_start(&mut started).await, "D");
        exec.run_to_completion().await.unwrap();
    }

    #[tokio::test]
    async fn add_unknown_tool_sets_provider_id_on_result() {
        let orch = ConversationOrchestrator::into_shared(orch_empty());
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "Ghost".into(),
            json!({}),
            Some("prov-abc-123".into()),
            MessageId::new(),
        )
        .await;
        let t = &exec.tools[0];
        let ContentBlock::ToolResult {
            provider_tool_use_id,
            ..
        } = t.result.as_ref().unwrap()
        else {
            panic!()
        };
        assert_eq!(provider_tool_use_id.as_deref(), Some("prov-abc-123"));
    }

    /// Part B: the UserInterrupted synthetic uses the BARE REJECT_MESSAGE with
    /// is_error: true, and is NOT `<tool_use_error>`-wrapped.
    #[test]
    fn user_interrupted_synthetic_is_bare_reject_message() {
        let block = synthetic_error_block(ToolUseId::new(), AbortReason::UserInterrupted);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        else {
            panic!()
        };
        assert!(
            is_error.unwrap_or(false),
            "user-interrupted result must be an error"
        );
        assert_eq!(content, REJECT_MESSAGE);
        assert!(
            !content.contains("<tool_use_error>"),
            "REJECT_MESSAGE must be bare (not tool_use_error-wrapped): {content}"
        );
    }

    #[test]
    fn tool_description_priority_and_truncation() {
        // command field, > 40 chars → truncate to 40 + ellipsis.
        let long = "x".repeat(45);
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "Bash".into(),
            input: json!({ "command": long }),
            provider_id: None,
            assistant_id: MessageId::new(),
            resolved_tool: None,
            status: ToolStatus::Queued,
            is_concurrency_safe: false,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
            publications: Vec::new(),
            dispatch_facts: None,
        };
        let desc = tool_description(&t);
        assert_eq!(desc, format!("Bash({}\u{2026})", "x".repeat(40)));

        // file_path fallback (no command), short → no truncation.
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "Read".into(),
            input: json!({ "file_path": "/tmp/a.txt" }),
            provider_id: None,
            assistant_id: MessageId::new(),
            resolved_tool: None,
            status: ToolStatus::Queued,
            is_concurrency_safe: true,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
            publications: Vec::new(),
            dispatch_facts: None,
        };
        assert_eq!(tool_description(&t), "Read(/tmp/a.txt)");

        // pattern fallback.
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "Grep".into(),
            input: json!({ "pattern": "foo" }),
            provider_id: None,
            assistant_id: MessageId::new(),
            resolved_tool: None,
            status: ToolStatus::Queued,
            is_concurrency_safe: true,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
            publications: Vec::new(),
            dispatch_facts: None,
        };
        assert_eq!(tool_description(&t), "Grep(foo)");

        // empty input → bare name.
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "SafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: MessageId::new(),
            resolved_tool: None,
            status: ToolStatus::Queued,
            is_concurrency_safe: true,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
            publications: Vec::new(),
            dispatch_facts: None,
        };
        assert_eq!(tool_description(&t), "SafeTool");
    }

    // ============================================================================
    // Task 9: take_newly_completed + has_unfinished tests
    // ============================================================================

    /// Two SafeTools driven to completion → take_newly_completed returns
    /// both in received order; a second call returns empty; statuses become Yielded.
    #[tokio::test]
    async fn take_newly_completed_returns_results_in_received_order() {
        let orch = ConversationOrchestrator::into_shared(orch_with_safe_tool());
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a)
            .await;
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a)
            .await;
        while !exec.inflight_is_empty() {
            exec.drain_one().await;
        }
        // Both should be Completed now.
        assert_eq!(exec.tools[0].status, ToolStatus::Completed);
        assert_eq!(exec.tools[1].status, ToolStatus::Completed);

        let results = exec.take_newly_completed();
        assert_eq!(results.len(), 2, "expected both tools drained");
        // Statuses should now be Yielded.
        assert_eq!(exec.tools[0].status, ToolStatus::Yielded);
        assert_eq!(exec.tools[1].status, ToolStatus::Yielded);

        // Second call returns empty (all already Yielded).
        let results2 = exec.take_newly_completed();
        assert!(results2.is_empty(), "second call must return empty");
    }

    #[tokio::test]
    async fn terminal_model_error_keeps_finished_results_and_synthesizes_unmatched_without_queueing()
     {
        let (orch, mut started, mut releases) = actor_queue_orch(&["executing-at-error"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant_id = MessageId::new();
        let completed_id = ToolUseId::from("completed-before-error");
        let queued_id = ToolUseId::from("queued-at-error");
        let executing_id = ToolUseId::from("executing-at-error");

        exec.add_tool(
            completed_id.clone(),
            "ActorQueue".into(),
            json!({"step":"completed-before-error", "safe":true, "hold":false}),
            None,
            assistant_id,
        )
        .await;
        exec.add_tool(
            executing_id.clone(),
            "ActorQueue".into(),
            json!({"step":"executing-at-error", "safe":false, "hold":true}),
            None,
            assistant_id,
        )
        .await;
        assert_eq!(
            next_actor_start(&mut started).await,
            "completed-before-error"
        );
        assert_eq!(next_actor_start(&mut started).await, "executing-at-error");

        assert_eq!(exec.drain_one().await, Some(0));
        let finished = exec.take_newly_completed();
        assert_eq!(finished.len(), 1);
        assert!(matches!(
            &finished[0].block,
            ContentBlock::ToolResult { tool_use_id, is_error: Some(false), .. }
                if tool_use_id.as_str() == completed_id.as_str()
        ));

        exec.add_tool(
            queued_id.clone(),
            "ActorQueue".into(),
            json!({"step":"queued-at-error", "safe":true, "hold":false}),
            Some("provider-queued".into()),
            assistant_id,
        )
        .await;
        exec.sync_statuses().await;
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_no_actor_start(&mut started).await;

        let (synthetics, removal) = exec.abandon_after_model_error("wire failed").await;
        assert_eq!(synthetics.len(), 2);
        assert_eq!(
            removal.ids,
            vec![completed_id, executing_id, queued_id.clone()]
        );
        assert!(removal.reason.is_none());
        assert!(exec.is_current_generation_idle().await);
        assert!(exec.tools.is_empty());
        assert!(exec.inflight_is_empty());
        let expected = terminal_error_tool_result("wire failed");
        for result in &synthetics {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                provider_tool_use_id,
                ..
            } = &result.block
            else {
                panic!("expected ToolResult block")
            };
            assert_eq!(content, &expected);
            assert!(is_error.unwrap_or(false));
            assert!(matches!(
                &result.tool_use_result,
                Some(serde_json::Value::String(text)) if text == &expected
            ));
            if tool_use_id.as_str() == queued_id.as_str() {
                assert_eq!(provider_tool_use_id.as_deref(), Some("provider-queued"));
            }
        }
        let _ = releases.remove("executing-at-error");
    }

    /// Unknown tools complete at admission and become visible at the next
    /// actor readiness scan; no execution queue pump is involved.
    #[tokio::test]
    async fn take_newly_completed_yields_unknown_tool_after_actor_scan() {
        let orch = ConversationOrchestrator::into_shared(orch_empty());
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "NoSuchTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        assert_eq!(exec.drain_ready().await, 1);
        assert_eq!(exec.tools[0].status, ToolStatus::Completed);

        let results = exec.take_newly_completed();
        assert_eq!(results.len(), 1, "unknown tool result should be drained");
        assert_eq!(exec.tools[0].status, ToolStatus::Yielded);

        let ContentBlock::ToolResult { is_error, .. } = &results[0].block else {
            panic!("expected ToolResult block")
        };
        assert!(
            is_error.unwrap_or(false),
            "unknown-tool block must be an error"
        );
        assert_eq!(results[0].post_tool_batch_calls.len(), 1);
        assert_eq!(
            results[0].post_tool_batch_calls[0].tool_response,
            Some(json!(
                "<tool_use_error>Error: No such tool available: NoSuchTool</tool_use_error>"
            )),
            "PostToolBatch covers every assistant tool_use and reads the yielded result content"
        );
    }

    /// A completed unknown tool behind a running unsafe call remains withheld
    /// from ordinary Tn until the barrier settles, then drains in registration order.
    #[tokio::test]
    async fn take_newly_completed_stops_at_executing_exclusive_tool() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["exclusive"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        let first_id = ToolUseId::new();
        let exclusive_id = ToolUseId::new();
        let tail_id = ToolUseId::new();

        exec.add_tool(
            first_id.clone(),
            "ActorQueue".into(),
            json!({"step":"first", "safe":true, "hold":false}),
            None,
            assistant,
        )
        .await;
        exec.add_tool(
            exclusive_id.clone(),
            "ActorQueue".into(),
            json!({"step":"exclusive", "safe":false, "hold":true}),
            None,
            assistant,
        )
        .await;
        assert_eq!(next_actor_start(&mut started).await, "first");
        assert_eq!(next_actor_start(&mut started).await, "exclusive");
        assert_eq!(exec.drain_one().await, Some(0));
        let first = exec.take_newly_completed();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0].block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str() == first_id.as_str()
        ));

        exec.add_tool(
            tail_id.clone(),
            "UnknownTail".into(),
            json!({}),
            None,
            assistant,
        )
        .await;
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        assert_eq!(exec.tools[2].status, ToolStatus::Completed);
        assert_eq!(exec.drain_ready().await, 0);
        assert!(exec.take_newly_completed().is_empty());

        release_actor_call(&mut releases, "exclusive").await;
        assert!(exec.drain_one().await.is_some());
        let tail = exec.take_newly_completed();
        assert_eq!(tail.len(), 2);
        assert!(matches!(
            &tail[0].block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str() == exclusive_id.as_str()
        ));
        assert!(matches!(
            &tail[1].block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str() == tail_id.as_str()
        ));
    }

    /// An executing safe tool does not block a later completed safe tool in Tn.
    #[tokio::test]
    async fn take_newly_completed_skips_executing_safe_and_emits_later_completed() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["safe-running"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let assistant = MessageId::new();
        let running_id = ToolUseId::new();
        let completed_id = ToolUseId::new();

        exec.add_tool(
            running_id.clone(),
            "ActorQueue".into(),
            json!({"step":"safe-running", "safe":true, "hold":true}),
            None,
            assistant,
        )
        .await;
        exec.add_tool(
            completed_id.clone(),
            "ActorQueue".into(),
            json!({"step":"safe-completed", "safe":true, "hold":false}),
            None,
            assistant,
        )
        .await;
        let mut starts = [
            next_actor_start(&mut started).await,
            next_actor_start(&mut started).await,
        ];
        starts.sort();
        assert_eq!(starts, ["safe-completed", "safe-running"]);

        assert!(exec.drain_one().await.is_some());
        let ready = exec.take_newly_completed();
        assert_eq!(ready.len(), 1);
        assert!(matches!(
            &ready[0].block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str() == completed_id.as_str()
        ));
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Yielded);

        release_actor_call(&mut releases, "safe-running").await;
        assert!(exec.drain_one().await.is_some());
        let finished = exec.take_newly_completed();
        assert_eq!(finished.len(), 1);
        assert!(matches!(
            &finished[0].block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str() == running_id.as_str()
        ));
    }

    // ============================================================================
    // DEFERRED-3: user-ESC granular interrupt (abort_reason_for + per-tool gating)
    // ============================================================================

    /// A concurrency-SAFE tool whose `interrupt_behavior()==Cancel`. Sleeps,
    /// racing its `ctx.cancel`; on cancel returns `Aborted` so the executor
    /// substitutes the synthetic. Mirrors a WebFetch/Agent-style Cancel tool.
    struct CancelBehaviorTool {
        name: &'static str,
        is_mcp: bool,
        started: Option<Arc<Notify>>,
        captured_cancel_tokens: Option<Arc<Mutex<Vec<tokio_util::sync::CancellationToken>>>>,
        concurrency_safe: bool,
    }

    #[async_trait]
    impl Tool for CancelBehaviorTool {
        fn name(&self) -> &str {
            self.name
        }
        fn is_mcp(&self) -> bool {
            self.is_mcp
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            self.concurrency_safe
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn interrupt_behavior(
            &self,
            _input: &serde_json::Value,
        ) -> tool_api::tool_trait::InterruptBehavior {
            tool_api::tool_trait::InterruptBehavior::Cancel
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
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
            _opts: &DescriptionOptions,
        ) -> String {
            "cancel-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            if let (Some(tokens), Some(token)) = (&self.captured_cancel_tokens, &ctx.cancel) {
                tokens.lock().unwrap().push(token.clone());
            }
            if let Some(started) = &self.started {
                started.notify_one();
            }
            let token = ctx.cancel.clone();
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    Ok(ToolCallResult {
                        data: json!({ "content": "cancel-tool-ran-to-end" }),
                        model_content: None,
                        new_messages: vec![],
                        context_modifier: None,
                        is_error: false,
                        mcp_meta: None,
                    })
                }
                () = async { match token { Some(t) => t.cancelled().await, None => std::future::pending().await } } => {
                    Err(ToolError::Aborted)
                }
            }
        }
    }

    fn orch_with_cancel_and_block_tools_started(
        cancel_started: Option<Arc<Notify>>,
    ) -> ConversationOrchestrator {
        orch_with_cancel_and_block_tools_started_with_output(
            cancel_started,
            Arc::new(MockOutputStream::new()),
        )
    }

    fn orch_with_cancel_and_block_tools_started_with_output(
        cancel_started: Option<Arc<Notify>>,
        output: Arc<dyn lingxi_core::host::OutputStream>,
    ) -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "CancelTool",
            is_mcp: false,
            started: cancel_started,
            captured_cancel_tokens: None,
            concurrency_safe: true,
        }) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "McpCancelTool",
            is_mcp: true,
            started: None,
            captured_cancel_tokens: None,
            concurrency_safe: true,
        }) as Arc<dyn Tool>);
        // SafeTool defaults to Block (no interrupt_behavior override).
        registry.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    fn orch_with_cancel_and_block_tools() -> ConversationOrchestrator {
        orch_with_cancel_and_block_tools_started(None)
    }

    /// `abort_reason_for`: a fired user-cancel token yields `UserInterrupted` for
    /// a Cancel-behavior tool and `None` for a Block-behavior tool (gating).
    #[tokio::test]
    async fn abort_reason_for_gates_on_interrupt_behavior() {
        let orch = ConversationOrchestrator::into_shared(orch_with_cancel_and_block_tools());
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a)
            .await;
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a)
            .await;
        // Not fired yet → no abort for either.
        assert_eq!(exec.abort_reason_for(0), None);
        assert_eq!(exec.abort_reason_for(1), None);
        user_cancel.cancel();
        assert_eq!(exec.abort_reason_for(0), Some(AbortReason::UserInterrupted));
        assert_eq!(
            exec.abort_reason_for(1),
            None,
            "Block tool is NOT interrupted"
        );
    }

    /// Queued Cancel-behavior tool under a fired user-cancel gets the bare
    /// REJECT_MESSAGE via `apply_abort_to_pending`; a queued Block tool runs.
    #[tokio::test]
    async fn apply_abort_to_pending_user_interrupt_rejects_cancel_tool_only() {
        let orch = ConversationOrchestrator::into_shared(orch_with_cancel_and_block_tools());
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a)
            .await;
        user_cancel.cancel();
        exec.apply_abort_to_pending().await;
        exec.run_to_completion().await.unwrap();
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = exec.tools[0].result.as_ref().unwrap()
        else {
            panic!()
        };
        assert!(is_error.unwrap_or(false));
        assert_eq!(content, REJECT_MESSAGE);
    }

    #[tokio::test]
    async fn interrupted_mcp_tool_uses_the_2_1_246_explicit_error() {
        let orch = ConversationOrchestrator::into_shared(orch_with_cancel_and_block_tools());
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        exec.add_tool(
            ToolUseId::new(),
            "McpCancelTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        user_cancel.cancel();
        exec.apply_abort_to_pending().await;
        exec.run_to_completion().await.unwrap();

        let ContentBlock::ToolResult {
            content, is_error, ..
        } = exec.tools[0].result.as_ref().unwrap()
        else {
            panic!()
        };
        assert!(is_error.unwrap_or(false));
        assert_eq!(
            content,
            "Error: The tool call was interrupted before a result was received. It may or may not have completed on the server — verify before assuming it succeeded, and retry if needed."
        );
    }

    /// End-to-end through `run_to_completion`: an in-flight Cancel-behavior tool
    /// observes its `ctx.cancel` (parented to the user token) firing mid-flight,
    /// returns early, and `drain_one` substitutes the bare REJECT_MESSAGE.
    #[tokio::test]
    async fn in_flight_cancel_tool_user_interrupted_gets_reject_message() {
        let started = Arc::new(Notify::new());
        let sink = Arc::new(MockOutputStream::new());
        let orch = ConversationOrchestrator::into_shared(
            orch_with_cancel_and_block_tools_started_with_output(
                Some(started.clone()),
                sink.clone(),
            ),
        );
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        orch.set_tool_frame_buffering(true).await;
        let tool_id = ToolUseId::new();
        exec.add_tool(tool_id.clone(), "CancelTool".into(), json!({}), None, a)
            .await;
        // Wait for the real Tool::call entry before firing user cancellation.
        started.notified().await;
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        user_cancel.cancel();
        let results = exec.run_to_completion().await.unwrap();
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        else {
            panic!()
        };
        assert!(is_error.unwrap_or(false));
        assert_eq!(
            content, REJECT_MESSAGE,
            "in-flight Cancel tool must get the bare REJECT_MESSAGE on user interrupt"
        );
        assert!(!content.contains("cancel-tool-ran-to-end"));
        let drained = exec.take_newly_completed();
        assert_eq!(drained.len(), 1);
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        orch.settle_stream_tool_results(
            &mut settlement,
            drained,
            &std::collections::HashMap::new(),
            &None,
        )
        .await;
        orch.set_tool_frame_buffering(false).await;

        let result_events = sink
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event {
                lingxi_core::host::OutputEvent::ToolResult { id, result, .. } if id == tool_id => {
                    Some(result)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            result_events,
            vec![json!({ "error": REJECT_MESSAGE })],
            "Tn emits exactly the accepted synthetic result, never the raw ToolError frame"
        );
        assert_eq!(
            sink.denial_snapshot().await,
            vec![(tool_id.clone(), "user-rejected".into())],
            "the accepted synthetic result retains its denial provenance"
        );
        assert_eq!(
            orch.transcript
                .tool_use_results
                .lock()
                .await
                .get(tool_id.as_str()),
            Some(&serde_json::Value::String("User rejected tool use".into())),
            "the accepted synthetic result remains in transcript metadata"
        );
    }

    /// Discard cooperatively cancels an entered W1 call and fences its late
    /// outcome from the next executor generation. It does not force-abort the
    /// future or synthesize a result after the attempt has been discarded.
    #[tokio::test]
    async fn discard_cancels_and_tombstones_in_flight_tool() {
        let started = Arc::new(Notify::new());
        let captured_cancel_tokens = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "CancelTool",
            is_mcp: false,
            started: Some(started.clone()),
            captured_cancel_tokens: Some(captured_cancel_tokens.clone()),
            concurrency_safe: true,
        }) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a)
            .await;
        // Wait for the real Tool::call entry, then discard while it is in flight.
        started.notified().await;
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        let token = captured_cancel_tokens.lock().unwrap()[0].clone();
        exec.discard().await;
        assert!(token.is_cancelled(), "discard signals the running W1 call");
        assert!(exec.tools.is_empty(), "discard tombstones old result rows");
        assert!(exec.take_newly_completed().is_empty());
        assert!(exec.is_current_generation_idle().await);
    }

    #[tokio::test]
    async fn server_fallback_cancels_old_work_and_reparents_fresh_work_to_user_cancel() {
        let started = Arc::new(Notify::new());
        let late_a_started = Arc::new(Notify::new());
        let release_late_a = Arc::new(Notify::new());
        let late_a_returned = Arc::new(Notify::new());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let queued_safe_call_entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured_cancel_tokens = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "CancelTool",
            is_mcp: false,
            started: Some(started.clone()),
            captured_cancel_tokens: Some(captured_cancel_tokens.clone()),
            concurrency_safe: false,
        }) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: late_a_started.clone(),
            release_first: release_late_a.clone(),
            call_returned: late_a_returned.clone(),
            return_context_layer: false,
        }) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(SafeTool {
            call_entries: Some(queued_safe_call_entries.clone()),
        }) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();

        let yielded_tool_id = ToolUseId::new();
        exec.add_tool(
            yielded_tool_id.clone(),
            "MissingTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        assert_eq!(exec.drain_ready().await, 1);
        assert_eq!(exec.take_newly_completed().len(), 1);

        // Unknown tools are already Completed, while SafeTool remains Queued.
        // Both must disappear together with the in-flight tool's old result slot.
        let completed_tool_id = ToolUseId::new();
        exec.add_tool(
            completed_tool_id.clone(),
            "MissingTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        let old_tool_id = ToolUseId::new();
        exec.add_tool(
            old_tool_id.clone(),
            "W1Capture".into(),
            json!({ "hold": true }),
            None,
            MessageId::new(),
        )
        .await;
        tokio::select! {
            _ = late_a_started.notified() => {}
            completed = exec.drain_one() => panic!("old call completed before the hop: {completed:?}"),
        }
        let old_token = captured.lock().unwrap()[0]
            .cancel_token
            .clone()
            .expect("old W1 receives its generation cancellation token");
        let queued_tool_id = ToolUseId::new();
        exec.add_tool(
            queued_tool_id.clone(),
            "SafeTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;

        assert_eq!(exec.tools[0].status, ToolStatus::Yielded);
        assert_eq!(exec.tools[1].status, ToolStatus::Completed);
        assert_eq!(exec.tools[2].status, ToolStatus::Executing);
        assert_eq!(exec.tools[3].status, ToolStatus::Queued);

        let removal = exec
            .reset_after_server_fallback(Some(ToolUseRemovalReason::FallbackSweep))
            .await;
        assert_eq!(
            removal,
            ToolUseRemoval {
                ids: vec![
                    yielded_tool_id,
                    completed_tool_id,
                    old_tool_id,
                    queued_tool_id
                ],
                reason: Some(ToolUseRemovalReason::FallbackSweep),
            },
            "the sweep reports every executor id in registration order"
        );
        assert!(!exec.tool_use_lifecycle.is_agent_idle());

        assert!(
            old_token.is_cancelled(),
            "the hop cancels old in-flight work"
        );
        assert!(
            exec.tools.is_empty(),
            "queued and completed old results are tombstoned"
        );
        assert!(exec.inflight_is_empty());
        assert!(
            exec.take_newly_completed().is_empty(),
            "old results cannot be drained"
        );
        assert_eq!(
            queued_safe_call_entries.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "queued SafeTool has not entered its real Tool::call after reset"
        );

        // A is deliberately a Block tool that ignores its cancelled token.
        // Let it return after reset and wait until the actor observes that old
        // dispatch completion before checking that queued B never entered W1.
        release_late_a.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            late_a_returned.notified(),
        )
        .await
        .expect("old W1 returns only after the explicit post-reset release");
        exec.scheduler
            .wait_for_observed_dispatch_completions(1)
            .await
            .expect("actor observes the late old-generation completion");
        assert_eq!(
            queued_safe_call_entries.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "late A completion cannot promote queued SafeTool into W1"
        );
        assert!(exec.tools.is_empty());
        assert!(exec.take_newly_completed().is_empty());

        // A post-hop tool gets a fresh child token. Parent cancellation still
        // reaches it, and it is classified as a user interruption rather than
        // as a stale server-fallback discard.
        exec.add_tool(
            ToolUseId::new(),
            "CancelTool".into(),
            json!({}),
            None,
            MessageId::new(),
        )
        .await;
        tokio::select! {
            _ = started.notified() => {}
            completed = exec.drain_one() => panic!("new call completed before cancellation: {completed:?}"),
        }
        let new_token = captured_cancel_tokens.lock().unwrap()[0].clone();
        assert!(
            !new_token.is_cancelled(),
            "the fresh executor token starts live"
        );
        user_cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), new_token.cancelled())
            .await
            .expect("the actor applies user cancellation to the fresh Cancel tool");
        assert!(
            new_token.is_cancelled(),
            "user cancellation still propagates to new work"
        );
        exec.drain_one()
            .await
            .expect("new call observes cancellation");
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = exec.tools[0].result.as_ref().expect("new tool result")
        else {
            panic!("expected a tool result");
        };
        assert!(is_error.unwrap_or(false));
        assert_eq!(content, REJECT_MESSAGE);
    }

    #[tokio::test]
    async fn declined_server_fallback_cancels_started_tool_and_retracts_attempt() {
        let started = Arc::new(Notify::new());
        let captured_cancel_tokens = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "CancelTool",
            is_mcp: false,
            started: Some(started.clone()),
            captured_cancel_tokens: Some(captured_cancel_tokens.clone()),
            concurrency_safe: false,
        }) as Arc<dyn Tool>);
        let mut config = OrchestratorConfig::default();
        config.server_fallback_regular_available_models = Some(vec!["requested-model".into()]);
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            config,
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        let assistant_id = MessageId::new();
        let fallback = HistoryEvent::ServerFallback {
            event: Box::new(
                lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent {
                    from_model: "requested-model".into(),
                    to_model: "fallback-model".into(),
                    reason: "sticky".into(),
                    api_refusal_category: None,
                    mid_stream: true,
                    request_id: Some("declined-request".into()),
                    discarded_blocks: vec![2],
                    retained_blocks: vec![0],
                    retained_text: "visible old answer".into(),
                    final_stop_reason: None,
                },
            ),
            profile: "anthropic-profile".into(),
            lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane {
                for_model: "requested-model".into(),
                model: "fallback-model".into(),
                mode: lingxi_llm_client::providers::anthropic::fallback_request::LaneMode::Explicit,
            },
        };
        let events = vec![
            Ok(message_start("declined-attempt", "requested-model")),
            Ok(content_block_start_text(0)),
            Ok(text_delta(0, "visible old answer")),
            Ok(content_block_stop(0)),
            Ok(content_block_start_tool_use(
                2,
                ToolUseId::from("in-flight-old-tool".to_owned()),
                "CancelTool",
            )),
            Ok(content_block_stop(2)),
            Ok(fallback),
            Ok(message_start("rejected-response", "fallback-model")),
            Ok(text_delta(3, "must never be delivered")),
            Ok(message_stop()),
        ];
        let fallback_gate = started.clone();
        let stream: futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>> =
            stream::iter(events)
                .then(move |event| {
                    let gate = fallback_gate.clone();
                    async move {
                        if matches!(&event, Ok(HistoryEvent::ServerFallback { .. })) {
                            gate.notified().await;
                        }
                        event
                    }
                })
                .boxed();
        let sink = Arc::new(MockOutputStream::new());
        let output: Arc<dyn lingxi_core::host::OutputStream> = sink.clone();

        let failure = crate::streaming_loop::pump_stream_with_executor_tracked_remaining(
            stream,
            &output,
            crate::streaming_loop::ExecutorPump {
                executor: &mut exec,
                assistant_id,
                query_history: Vec::new(),
                model_profile: None,
                record_supersedes: false,
                user_cancel: Some(&user_cancel),
                suppress_live_text: false,
                suppress_live_thinking: false,
                settlement: None,
            },
        )
        .await
        .map(|(turn, _remaining)| turn)
        .expect_err("fallback model is outside this session's allowlist");

        assert_eq!(
            failure.disposition,
            crate::streaming_loop::PumpFailureDisposition::ServerFallbackDeclined
        );
        assert!(failure.partial.assistant_blocks.is_empty());
        assert!(failure.partial.tool_uses.is_empty());
        assert_eq!(failure.partial.server_fallback_events.len(), 1);
        assert_eq!(
            failure.partial.tool_use_removals,
            vec![ToolUseRemoval {
                ids: vec![ToolUseId::from("in-flight-old-tool")],
                reason: None,
            }],
            "declining a hop removes exact ids without a fallback-sweep reason"
        );
        let token = captured_cancel_tokens.lock().unwrap()[0].clone();
        assert!(
            token.is_cancelled(),
            "declining the hop aborts the old tool token"
        );
        assert!(exec.tools.is_empty());
        assert!(exec.inflight_is_empty());
        assert_eq!(sink.text_events().await, ["visible old answer"]);
        assert!(sink.snapshot().await.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::MessageRetracted { message_id }
                if message_id == &assistant_id
        )));
    }

    /// `has_unfinished` stays true through real execution and completion, then
    /// becomes false only after the caller yields the ready result.
    #[tokio::test]
    async fn has_unfinished_tracks_non_yielded_tools() {
        let (orch, mut started, mut releases) = actor_queue_orch(&["A"]);
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        // No tools at all → nothing unfinished.
        assert!(
            !exec.has_unfinished(),
            "empty executor must have no unfinished tools"
        );

        add_actor_queue_call(&mut exec, "A", true, true, MessageId::new()).await;
        assert_eq!(next_actor_start(&mut started).await, "A");
        assert!(exec.has_unfinished(), "executing tool remains unfinished");
        release_actor_call(&mut releases, "A").await;
        assert_eq!(exec.drain_one().await, Some(0));
        assert!(
            exec.has_unfinished(),
            "Completed but not yet Yielded still unfinished"
        );

        exec.take_newly_completed();
        assert!(!exec.has_unfinished(), "all Yielded → no unfinished tools");
    }

    #[tokio::test]
    async fn streamed_tool_awaits_append_but_dispatches_original_query_block() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("append-order.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              const ref = { plugin: 'append-order', key: 'order' };
              on('session.append', async ($, event, next) => {
                if (event.message.type !== 'assistant'
                    || event.message.content[0]?.type !== 'tool_use') return next(event);
                const prior = (await $.state.get(ref)).value;
                await $.state.set(ref, {
                  appendCount: (prior?.appendCount ?? 0) + 1,
                  uuid: event.uuid, stage: 'appended'
                });
                return next({ ...event, message: { ...event.message,
                  content: [{ type: 'text', text: 'stored replacement' }]
                } });
              });
              on('tool.call', async ($, event, next) => {
                const observed = (await $.state.get(ref)).value;
                if (event.tool === 'Probe') return { result: observed };
                if (event.tool === 'SafeTool') {
                  if (observed?.stage !== 'appended') throw new Error('tool preceded append');
                  await $.state.set(ref, { ...observed, stage: 'called', tool: event.tool });
                }
                return next(event);
              });
            }
            "#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("append-order", dir.path(), &module, json!({}))
            .await
            .unwrap();
        let mut hooks = hooks::HookRegistry::new();
        hooks.set_mod_host(host.clone());
        let orch = ConversationOrchestrator::into_shared(
            orch_with_safe_tool().with_hook_registry(Arc::new(tokio::sync::RwLock::new(hooks))),
        );
        let id = ToolUseId::from("original-tool");
        let output: Arc<dyn lingxi_core::host::OutputStream> = Arc::new(MockOutputStream::new());
        let events = vec![
            Ok(message_start("streamed-append", "model")),
            Ok(content_block_start_tool_use(0, id.clone(), "SafeTool")),
            Ok(content_block_stop(0)),
            Ok(crate::test_support_stream::message_delta_stop("tool_use")),
            Ok(message_stop()),
        ];
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let (turn, _) = crate::streaming_loop::pump_stream_with_executor_tracked_remaining(
            stream::iter(events).boxed(),
            &output,
            crate::streaming_loop::ExecutorPump {
                executor: &mut executor,
                assistant_id: MessageId::new(),
                query_history: Vec::new(),
                model_profile: None,
                record_supersedes: false,
                user_cancel: None,
                suppress_live_text: false,
                suppress_live_thinking: false,
                settlement: None,
            },
        )
        .await
        .unwrap();
        while !executor.inflight_is_empty() {
            executor.drain_one().await;
        }
        assert_eq!(executor.tools.len(), 1);
        assert_eq!(executor.tools[0].id, id);
        assert_eq!(executor.tools[0].name, "SafeTool");
        let row = &turn.assistant_rows[0];
        assert_eq!(executor.tools[0].assistant_id, row.row_id);
        assert!(matches!(
            row.content.as_slice(),
            [
                ContentBlock::Text { text, .. },
                ContentBlock::ToolUse { id: accepted_id, name, input, .. }
            ] if text == "stored replacement"
                && accepted_id == &id
                && name == "SafeTool"
                && input == &json!({})
        ));
        assert_eq!(
            turn.assistant_blocks,
            vec![ContentBlock::Text {
                text: "stored replacement".into(),
                citations: Some(None),
            }],
            "accepted Text enters the pump's query content without duplicating source ToolUse"
        );
        let observed = host
            .dispatch(
                "tool.call",
                json!({"tool":"Probe", "input":{}}),
                |_| async { panic!("the probe is answered by the Mod") },
            )
            .await
            .unwrap();
        assert_eq!(observed["result"]["stage"], "called");
        assert_eq!(observed["result"]["appendCount"], 1);
        assert_eq!(observed["result"]["uuid"], row.row_id.as_uuid().to_string());
        let ContentBlock::ToolResult { is_error, .. } = executor.tools[0].result.as_ref().unwrap()
        else {
            panic!("expected the original tool result");
        };
        assert!(!is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn live_stream_tool_context_does_not_cross_assistant_rows() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: first_started.clone(),
            release_first: release_first.clone(),
            call_returned: Arc::new(Notify::new()),
            return_context_layer: false,
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let query_history = vec![lingxi_core::types::ConversationMessage::user(
            MessageId::new(),
            "frozen before this query".into(),
        )];
        let first_id = ToolUseId::new();
        let second_id = ToolUseId::new();
        let events = vec![
            Ok(message_start("w1-context", "model")),
            Ok(content_block_start_tool_use(
                0,
                first_id.clone(),
                "W1Capture",
            )),
            Ok(input_json_delta(0, r#"{"hold":true}"#)),
            Ok(content_block_stop(0)),
            Ok(content_block_start_tool_use(
                1,
                second_id.clone(),
                "W1Capture",
            )),
            Ok(input_json_delta(1, r#"{"hold":false}"#)),
            Ok(content_block_stop(1)),
            Ok(crate::test_support_stream::message_delta_stop("tool_use")),
            Ok(message_stop()),
        ];
        let output: Arc<dyn lingxi_core::host::OutputStream> = Arc::new(MockOutputStream::new());
        let mut executor = StreamingToolExecutor::try_new(&orch, query_history.clone())
            .await
            .unwrap();
        let pump = crate::streaming_loop::pump_stream_with_executor_tracked_remaining(
            stream::iter(events).boxed(),
            &output,
            crate::streaming_loop::ExecutorPump {
                executor: &mut executor,
                assistant_id: MessageId::new(),
                query_history: query_history.clone(),
                model_profile: None,
                record_supersedes: false,
                user_cancel: None,
                suppress_live_text: false,
                suppress_live_thinking: false,
                settlement: None,
            },
        );
        let (turn, _) = tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "stage=stream-pump timed out; captured_calls={}; tracked={:?}; inflight={}",
                    captured.lock().unwrap().len(),
                    executor
                        .tools
                        .iter()
                        .map(|tool| tool.status)
                        .collect::<Vec<_>>(),
                    !executor.inflight_is_empty(),
                )
            })
            .expect("streamed assistant turn completes");

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                tokio::select! {
                    _ = first_started.notified() => {}
                    completed = executor.drain_one() => {
                        panic!("held first tool completed before release: {completed:?}");
                    }
                }
            },
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "stage=first-tool-start timed out after pump returned; captured_calls={}; tracked={:?}; inflight={}",
                captured.lock().unwrap().len(),
                executor.tools.iter().map(|tool| tool.status).collect::<Vec<_>>(),
                !executor.inflight_is_empty(),
            )
        });
        // Change live session history after the second tool is registered but
        // before its exclusive predecessor releases the queue. The second
        // invocation must still see the exact query snapshot captured above.
        orch.session
            .lock()
            .await
            .history
            .push(lingxi_core::types::ConversationMessage::user(
                MessageId::new(),
                "arrived after query start".into(),
            ));
        release_first.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), executor.drain_one())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "stage=first-tool-drain timed out; captured_calls={}; tracked={:?}; inflight={}",
                    captured.lock().unwrap().len(),
                    executor.tools.iter().map(|tool| tool.status).collect::<Vec<_>>(),
                    !executor.inflight_is_empty(),
                )
            })
            .expect("first tool completes");
        while !executor.inflight_is_empty() {
            tokio::time::timeout(std::time::Duration::from_secs(5), executor.drain_one())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "stage=remaining-tool-drain timed out; captured_calls={}; tracked={:?}; inflight={}",
                        captured.lock().unwrap().len(),
                        executor.tools.iter().map(|tool| tool.status).collect::<Vec<_>>(),
                        !executor.inflight_is_empty(),
                    )
                });
        }

        assert_eq!(turn.assistant_rows.len(), 2);
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].messages, query_history);
        assert_eq!(captured[1].messages, query_history);
        assert_eq!(
            captured[0]
                .assistant_message
                .as_ref()
                .map(|message| message.id()),
            Some(turn.assistant_rows[0].row_id),
            "first call receives its raw completed assistant row separately"
        );
        assert_eq!(
            captured[1]
                .assistant_message
                .as_ref()
                .map(|message| message.id()),
            Some(turn.assistant_rows[1].row_id),
            "second call receives its own raw completed assistant row separately"
        );
        assert_ne!(
            captured[0].assistant_message_id, captured[1].assistant_message_id,
            "each streamed ToolUse stop creates a distinct assistant row identity"
        );
        assert!(captured[0].same_turn_tool_uses.is_empty());
        assert!(
            captured[1].same_turn_tool_uses.is_empty(),
            "Native sibling facts do not cross distinct streamed assistantMessage rows"
        );
        assert!(matches!(
            captured[0].assistant_message.as_ref(),
            Some(lingxi_core::types::ConversationMessage::Assistant { content, .. })
                if matches!(content.as_slice(), [ContentBlock::ToolUse { id, .. }] if id == &first_id)
        ));
        assert!(matches!(
            captured[1].assistant_message.as_ref(),
            Some(lingxi_core::types::ConversationMessage::Assistant { content, .. })
                if matches!(content.as_slice(), [ContentBlock::ToolUse { id, .. }] if id == &second_id)
        ));
    }

    #[tokio::test]
    async fn recovered_full_row_shares_siblings_and_preserves_content_ordinals() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: Arc::new(Notify::new()),
            release_first: Arc::new(Notify::new()),
            call_returned: Arc::new(Notify::new()),
            return_context_layer: false,
        }));
        let first_id = "recovered-a";
        let second_id = "recovered-b";
        let recovered = mock_message_response(
            vec![
                llm_runtime::ContentBlock::Text {
                    text: "prefix".into(),
                    cache_control: None,
                    citations: Some(None),
                },
                llm_runtime::ContentBlock::ToolCall { input_projection: None,
                    id: first_id.into(),
                    name: "W1Capture".into(),
                    input: json!({ "hold": false }),
                },
                llm_runtime::ContentBlock::ToolCall { input_projection: None,
                    id: second_id.into(),
                    name: "W1Capture".into(),
                    input: json!({ "hold": false }),
                },
            ],
            Some("tool_use"),
        );
        let completed = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: Some(None),
            }],
            Some("end_turn"),
        );
        let streaming = Arc::new(MockStreamingApiClient::with_open_error(
            LlmError::ContextOverflow { token_gap: 100 },
            Vec::new(),
        ));
        let api = Arc::new(MockApiClient::new(vec![recovered, completed]));
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: "requested-model".into(),
                    ..OrchestratorConfig::default()
                },
                api,
                streaming,
                Arc::new(registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            ));
        orch.run_turn_streaming("recover full row")
            .await
            .expect("prompt-too-long recovery dispatches the full assistant row");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let first = captured
            .iter()
            .find(|call| {
                call.tool_use_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == first_id)
            })
            .expect("first recovered ToolUse reaches W1");
        let second = captured
            .iter()
            .find(|call| {
                call.tool_use_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == second_id)
            })
            .expect("second recovered ToolUse reaches W1");
        assert_eq!(first.assistant_message_id, second.assistant_message_id);
        assert!(first.same_turn_tool_uses.is_empty());
        assert_eq!(
            second.same_turn_tool_uses,
            vec![ContentBlock::ToolUse { input_projection: None,
                id: ToolUseId::from(first_id),
                name: "W1Capture".into(),
                input: json!({ "hold": false }),
                provider_id: None,
            }]
        );
        for call in [first, second] {
            assert!(
                matches!(
                    call.assistant_message.as_ref(),
                    Some(lingxi_core::types::ConversationMessage::Assistant { content, .. })
                        if matches!(content.as_slice(), [
                            ContentBlock::Text { text, .. },
                            ContentBlock::ToolUse { id: first, .. },
                            ContentBlock::ToolUse { id: second, .. },
                        ] if text == "prefix"
                            && first.as_str() == first_id
                            && second.as_str() == second_id)
                ),
                "recovered calls share one full assistant row with original content positions"
            );
        }
    }

    #[tokio::test]
    async fn live_pump_settles_ready_results_into_raw_je_and_event_journal() {
        let orch = ConversationOrchestrator::into_shared(orch_with_safe_tool());
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        let tool_id = ToolUseId::new();
        let events = vec![
            Ok(message_start("live-settlement", "model")),
            Ok(content_block_start_tool_use(
                0,
                tool_id.clone(),
                "NotRegistered",
            )),
            Ok(input_json_delta(0, "{}")),
            Ok(content_block_stop(0)),
            Ok(content_block_start_text(1)),
            Ok(text_delta(1, "assistant tail")),
            Ok(content_block_stop(1)),
            Ok(crate::test_support_stream::message_delta_stop("tool_use")),
            Ok(message_stop()),
        ];
        let output: Arc<dyn lingxi_core::host::OutputStream> = Arc::new(MockOutputStream::new());
        let (turn, _) = crate::streaming_loop::pump_stream_with_executor_tracked_remaining(
            stream::iter(events).boxed(),
            &output,
            crate::streaming_loop::ExecutorPump {
                executor: &mut executor,
                assistant_id: MessageId::new(),
                query_history: Vec::new(),
                model_profile: None,
                record_supersedes: false,
                user_cancel: None,
                suppress_live_text: false,
                suppress_live_thinking: false,
                settlement: Some(&mut settlement),
            },
        )
        .await
        .expect("streamed assistant turn completes");

        assert_eq!(turn.assistant_rows.len(), 2);
        assert_eq!(settlement.query_rows.len(), 1);
        assert!(matches!(
            &settlement.query_rows[0].0,
            lingxi_core::types::ConversationMessage::User { content, .. }
                if matches!(content.as_slice(), [ContentBlock::ToolResult { tool_use_id: id, .. }] if id == &tool_id)
        ));
        assert!(orch.session.lock().await.history.is_empty());
        assert!(matches!(
            settlement.journal.as_slice(),
            [
                crate::streaming_loop::StreamEventJournalEntry::AssistantRow(first),
                crate::streaming_loop::StreamEventJournalEntry::UserRow { parent_uuid: Some(parent), .. },
                crate::streaming_loop::StreamEventJournalEntry::FlushHookAttachments(flushed),
                crate::streaming_loop::StreamEventJournalEntry::AssistantRow(last),
            ] if *first == turn.assistant_rows[0].row_id
                && parent == &first.as_uuid().to_string()
                && flushed == &tool_id
                && *last == turn.assistant_rows[1].row_id
        ));
    }

    #[tokio::test]
    async fn live_stream_advances_serial_tools_but_hands_off_after_provider_event() {
        let release_first = Arc::new(Notify::new());
        let second_started = Arc::new(Notify::new());
        let release_second = Arc::new(Notify::new());
        let events_seen = Arc::new(Mutex::new(Vec::new()));
        let output: Arc<dyn lingxi_core::host::OutputStream> =
            Arc::new(ReleaseFirstOnSecondAssistantRow {
                release_first: release_first.clone(),
                release_second: release_second.clone(),
                events: events_seen.clone(),
            });
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(StreamBoundarySerialTool {
            release_first: release_first.clone(),
            second_started: second_started.clone(),
            release_second: release_second.clone(),
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        orch.set_tool_frame_buffering(true).await;
        let first_id = ToolUseId::new();
        let second_id = ToolUseId::new();
        let events = vec![
            Ok(message_start("serial-event-clock", "model")),
            Ok(content_block_start_tool_use(
                0,
                first_id.clone(),
                "StreamBoundarySerial",
            )),
            Ok(input_json_delta(0, r#"{"step":"first"}"#)),
            Ok(content_block_stop(0)),
            Ok(content_block_start_tool_use(
                1,
                second_id.clone(),
                "StreamBoundarySerial",
            )),
            Ok(input_json_delta(1, r#"{"step":"second"}"#)),
            Ok(content_block_stop(1)),
            Ok(content_block_start_text(2)),
        ];
        let text_delta_after_first_completion = async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), second_started.notified())
                .await
                .expect("bounded stage=second-serial-tool-start");
            // B can start only after A is completed and available to Tn.
            // This provider delta has no completed assistant row, so A is
            // settled before the following text block stop creates row C.
            Ok(text_delta(2, "event row"))
        };
        let after_gate = vec![
            Ok(content_block_stop(2)),
            Ok(crate::test_support_stream::message_delta_stop("tool_use")),
            Ok(message_stop()),
        ];
        let stream = stream::iter(events)
            .chain(stream::once(text_delta_after_first_completion))
            .chain(stream::iter(after_gate))
            .boxed();
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        let (turn, _) = crate::streaming_loop::pump_stream_with_executor_tracked_remaining(
            stream,
            &output,
            crate::streaming_loop::ExecutorPump {
                executor: &mut executor,
                assistant_id: MessageId::new(),
                query_history: Vec::new(),
                model_profile: None,
                record_supersedes: false,
                user_cancel: None,
                suppress_live_text: false,
                suppress_live_thinking: false,
                settlement: Some(&mut settlement),
            },
        )
        .await
        .expect("gated provider event is released by the second unsafe tool");

        // The stream pump only settles results that are ready at a provider
        // event boundary. The driver drains any remaining executor work after
        // this response, so mirror that handoff before inspecting the complete
        // query result sequence.
        loop {
            let newly_completed = executor.take_newly_completed();
            orch.settle_stream_tool_results(
                &mut settlement,
                newly_completed,
                &std::collections::HashMap::new(),
                &None,
            )
            .await;
            if executor.inflight_is_empty() {
                break;
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), executor.drain_one())
                .await
                .expect("bounded stage=post-response-tool-drain");
        }

        assert_eq!(turn.assistant_rows.len(), 3);
        let result_ids: Vec<_> = settlement
            .query_rows
            .iter()
            .filter_map(|(row, _)| match row {
                lingxi_core::types::ConversationMessage::User { content, .. } => {
                    content.iter().find_map(|block| match block {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect();
        assert_eq!(result_ids, [first_id.clone(), second_id.clone()]);
        // Native Xl handles each provider event before Tn. The text delta
        // above is released only after B starts, proving A is ready at that
        // boundary. Row C then releases B, fixing both result positions.
        let expected_journal_order = matches!(
            settlement.journal.as_slice(),
            [
                crate::streaming_loop::StreamEventJournalEntry::AssistantRow(first),
                crate::streaming_loop::StreamEventJournalEntry::AssistantRow(second),
                crate::streaming_loop::StreamEventJournalEntry::UserRow { .. },
                crate::streaming_loop::StreamEventJournalEntry::FlushHookAttachments(first_flushed),
                crate::streaming_loop::StreamEventJournalEntry::AssistantRow(event_row),
                crate::streaming_loop::StreamEventJournalEntry::UserRow { .. },
                crate::streaming_loop::StreamEventJournalEntry::FlushHookAttachments(second_flushed),
            ] if *first == turn.assistant_rows[0].row_id
                && *second == turn.assistant_rows[1].row_id
                && *event_row == turn.assistant_rows[2].row_id
                && first_flushed == &first_id
                && second_flushed == &second_id
        );
        assert!(
            expected_journal_order,
            "unexpected complete stream journal order: {:#?}",
            settlement.journal
        );
        assert_eq!(
            *events_seen.lock().unwrap(),
            [
                "assistant:0".to_string(),
                "assistant:1".to_string(),
                format!("result:{first_id}"),
                "assistant:2".to_string(),
                format!("result:{second_id}"),
            ],
            "the first completion releases the queued second call; the pump settles provider-event-ready results, then the driver tail settles remaining work without changing journal order; journal={:#?}",
            settlement.journal
        );
    }

    fn with_context_layer_model_routes(
        orch: ConversationOrchestrator,
        models: &[&str],
    ) -> ConversationOrchestrator {
        let registered_model_ids: std::collections::BTreeSet<String> =
            std::iter::once(orch.config.model.clone())
                .chain(models.iter().map(|model| (*model).to_owned()))
                .collect();
        orch.with_model_resolution_context_provider(Arc::new(
            move |model: &str, profile: Option<&str>| {
                if !registered_model_ids.contains(model) {
                    return Err(
                        agent::model_resolution::ModelResolutionError::RouteUnavailable {
                            model: model.to_owned(),
                            profile: profile.map(str::to_owned),
                            reason: "model is absent from the context-layer fixture catalog".into(),
                        },
                    );
                }
                Ok(agent::model_resolution::ModelResolutionContext {
                    route: agent::model_resolution::ModelRouteFacts {
                        model: model.to_owned(),
                        profile: profile.map(str::to_owned),
                        provider: Some(agent::model_resolution::ModelProviderKind::Other),
                        ..Default::default()
                    },
                    registered_model_ids: registered_model_ids.clone(),
                    ..Default::default()
                })
            },
        ))
    }

    #[tokio::test]
    async fn owned_add_reaches_real_tool_call_and_carries_full_context_layer() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let call_returned = Arc::new(Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: first_started.clone(),
            release_first: release_first.clone(),
            call_returned: call_returned.clone(),
            return_context_layer: true,
        }));
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let orch = ConversationOrchestrator::into_shared(with_context_layer_model_routes(
            orch,
            &["layered-model"],
        ));
        let query_history = vec![lingxi_core::types::ConversationMessage::user(
            MessageId::new(),
            "frozen request history".into(),
        )];
        let mut executor = StreamingToolExecutor::try_new(&orch, query_history.clone())
            .await
            .expect("production-owned scheduler has its composition-root owner");
        let first_id = ToolUseId::new();
        let second_id = ToolUseId::new();
        let assistant_id = MessageId::new();
        let first_input = json!({
            "hold": true,
            "layered_cwd": "/modifier-owned-cwd",
            "layered_permission_mode": "plan",
        });
        let second_input = json!({"hold": false});
        let full_row = lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
            id: assistant_id.clone(),
            content: vec![
                ContentBlock::Text {
                    text: "prefix before tools".into(),
                    citations: Some(None),
                },
                ContentBlock::ToolUse { input_projection: None,
                    id: first_id.clone(),
                    name: "W1Capture".into(),
                    input: first_input.clone(),
                    provider_id: None,
                },
                ContentBlock::ToolUse { input_projection: None,
                    id: second_id.clone(),
                    name: "W1Capture".into(),
                    input: second_input.clone(),
                    provider_id: None,
                },
            ],
            stop_reason: None,
        };

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            executor.add_tool_with_context_owned(
                first_id.clone(),
                "W1Capture".into(),
                first_input,
                None,
                assistant_id.clone(),
                crate::turn_loop::ToolUseDispatchFacts {
                    query_history: query_history.clone(),
                    assistant_message: full_row.clone(),
                    same_turn_tool_uses: Vec::new(),
                },
            ),
        )
        .await
        .expect("bounded stage=owned-add-first")
        .expect("Add reaches the production dispatch entry");
        tokio::time::timeout(std::time::Duration::from_secs(5), first_started.notified())
            .await
            .expect("bounded stage=real-W1-tool-call-start");
        assert_eq!(captured.lock().unwrap().len(), 1);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            executor.add_tool_with_context_owned(
                second_id.clone(),
                "W1Capture".into(),
                second_input,
                None,
                assistant_id.clone(),
                crate::turn_loop::ToolUseDispatchFacts {
                    query_history: query_history.clone(),
                    assistant_message: full_row,
                    same_turn_tool_uses: vec![ContentBlock::ToolUse { input_projection: None,
                        id: first_id.clone(),
                        name: "W1Capture".into(),
                        input: json!({"hold": true}),
                        provider_id: None,
                    }],
                },
            ),
        )
        .await
        .expect("bounded stage=owned-add-second")
        .expect("queued Add is accepted without a provider/Tn poll");
        assert_eq!(captured.lock().unwrap().len(), 1);

        release_first.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), call_returned.notified())
            .await
            .expect("bounded stage=second-real-W1-call");
        let mut ready_count = 0;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !executor.is_current_generation_idle().await {
                ready_count += executor.drain_ready().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bounded stage=owned-results-final-drain");
        assert_eq!(ready_count, 2);
        {
            let captured = captured.lock().unwrap();
            assert_eq!(captured.len(), 2);
            assert_eq!(captured[0].messages, query_history);
            assert_eq!(captured[1].messages, query_history);
            assert_eq!(captured[0].assistant_message_id, Some(assistant_id.clone()));
            assert_eq!(captured[1].assistant_message_id, Some(assistant_id.clone()));
            assert_eq!(captured[1].tool_use_id.as_ref(), Some(&second_id));
            for call in captured.iter() {
                assert!(
                    matches!(
                        call.assistant_message.as_ref(),
                        Some(lingxi_core::types::ConversationMessage::Assistant { id, content, .. })
                            if id == &assistant_id
                                && matches!(content.as_slice(), [
                                    ContentBlock::Text { text, .. },
                                    ContentBlock::ToolUse { id: first, .. },
                                    ContentBlock::ToolUse { id: second, .. },
                                ] if text == "prefix before tools" && first == &first_id && second == &second_id)
                    ),
                    "recovered ToolUses share one complete assistant row and preserve content ordinals"
                );
            }
            assert_eq!(
                captured[1].same_turn_tool_uses,
                vec![ContentBlock::ToolUse { input_projection: None,
                    id: first_id.clone(),
                    name: "W1Capture".into(),
                    input: json!({"hold": true}),
                    provider_id: None,
                }]
            );
            assert_eq!(
                captured[0].session_ptr, captured[1].session_ptr,
                "the physical session identity survives context-layer propagation"
            );
            assert_eq!(
                captured[0].registry_ptr, captured[1].registry_ptr,
                "the physical tool registry identity survives context-layer propagation"
            );
            assert!(captured[0].session_ptr.is_some());
            assert!(captured[0].registry_ptr.is_some());
            assert_eq!(captured[0].cwd, None, "main calls use shared session cwd");
            assert_eq!(
                captured[0].trusted_effective_permission_mode, None,
                "the main call has no trusted per-agent mode override"
            );
            assert!(captured[0].has_instruction_context);
            assert_eq!(
                captured[1].cwd,
                Some(PathBuf::from("/modifier-owned-cwd")),
                "the one-shot layer's cwd reaches the next W1 context"
            );
            assert_eq!(
                captured[1].trusted_effective_permission_mode.as_deref(),
                Some("plan"),
                "the one-shot layer's permission fact reaches the next W1 context"
            );
            assert_eq!(captured[1].main_loop_model, "layered-model");
            assert!(captured[1].verbose);
        }
        assert_eq!(executor.tools[0].status, ToolStatus::Completed);
        assert_eq!(executor.tools[1].status, ToolStatus::Completed);
        assert!(executor.tools.iter().all(|tool| tool.result.is_some()));
        assert_eq!(executor.take_newly_completed().len(), 2);
    }

    #[tokio::test]
    async fn rebuilt_executor_restores_constructor_context_before_new_layer() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let call_returned = Arc::new(Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: first_started.clone(),
            release_first: release_first.clone(),
            call_returned: call_returned.clone(),
            return_context_layer: true,
        }));
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig {
                model: "reset-base-model".into(),
                ..OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let orch = ConversationOrchestrator::into_shared(with_context_layer_model_routes(
            orch,
            &["old-generation-layer", "new-generation-layer"],
        ));
        let query_history = vec![lingxi_core::types::ConversationMessage::user(
            MessageId::new(),
            "constructor history".into(),
        )];
        let assistant_id = MessageId::new();
        let first_id = ToolUseId::new();
        let second_id = ToolUseId::new();
        let mut executor = StreamingToolExecutor::try_new(&orch, query_history.clone())
            .await
            .unwrap();

        let first_input = json!({"hold": true, "layered_model": "old-generation-layer"});
        let first_row = lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
            id: assistant_id,
            content: vec![ContentBlock::ToolUse { input_projection: None,
                id: first_id.clone(),
                name: "W1Capture".into(),
                input: first_input.clone(),
                provider_id: None,
            }],
            stop_reason: None,
        };
        executor
            .add_tool_with_context_owned(
                first_id.clone(),
                "W1Capture".into(),
                first_input,
                None,
                assistant_id,
                crate::turn_loop::ToolUseDispatchFacts {
                    query_history: query_history.clone(),
                    assistant_message: first_row,
                    same_turn_tool_uses: Vec::new(),
                },
            )
            .await
            .unwrap();
        first_started.notified().await;
        release_first.notify_one();
        assert_eq!(executor.drain_one().await, Some(0));
        assert_eq!(executor.take_newly_completed().len(), 1);
        assert_eq!(
            captured.lock().unwrap()[0].main_loop_model,
            "reset-base-model"
        );

        // An unsafe modifier has already updated the old executor's current
        // context. Native rebuild creates a new executor from the immutable
        // query-owned base, so this generation must not inherit that layer.
        let removal = executor
            .reset_after_server_fallback_owned(None)
            .await
            .unwrap();
        assert_eq!(removal.ids, vec![first_id]);
        assert!(executor.tools.is_empty());

        let second_input = json!({"hold": false, "layered_model": "new-generation-layer"});
        let second_row = lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
            id: assistant_id,
            content: vec![ContentBlock::ToolUse { input_projection: None,
                id: second_id.clone(),
                name: "W1Capture".into(),
                input: second_input.clone(),
                provider_id: None,
            }],
            stop_reason: None,
        };
        executor
            .add_tool_with_context_owned(
                second_id,
                "W1Capture".into(),
                second_input,
                None,
                assistant_id,
                crate::turn_loop::ToolUseDispatchFacts {
                    query_history: query_history.clone(),
                    assistant_message: second_row,
                    same_turn_tool_uses: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(executor.drain_one().await, Some(0));
        assert_eq!(executor.take_newly_completed().len(), 1);
        executor.finish_context_layers().await.unwrap();

        let final_model = orch.session.lock().await.model.clone();
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].messages, query_history);
        assert_eq!(captured[1].messages, query_history);
        assert_eq!(captured[0].main_loop_model, "reset-base-model");
        assert_eq!(
            captured[1].main_loop_model, "reset-base-model",
            "the rebuilt executor starts from its constructor-owned context"
        );
        assert!(!captured[0].verbose);
        assert!(!captured[1].verbose);
        assert_eq!(
            final_model, "new-generation-layer",
            "the post-reset modifier is applied successfully to the rebuilt base"
        );
    }

    fn visible_sticky_fallback() -> llm_runtime::history::HistoryServerFallback {
        llm_runtime::history::HistoryServerFallback {
            event:
                lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent {
                    from_model: "requested-model".into(),
                    to_model: "fallback-model".into(),
                    reason: "sticky".into(),
                    api_refusal_category: None,
                    mid_stream: true,
                    request_id: Some("accepted-no-tool-fallback".into()),
                    discarded_blocks: Vec::new(),
                    retained_blocks: Vec::new(),
                    retained_text: String::new(),
                    final_stop_reason: None,
                },
            profile: "fallback-profile".into(),
            lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane {
                for_model: "requested-model".into(),
                model: "fallback-model".into(),
                mode: lingxi_llm_client::providers::anthropic::fallback_request::LaneMode::Explicit,
            },
        }
    }

    #[tokio::test]
    async fn no_tool_visible_server_fallback_keeps_the_accepted_session_model() {
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig {
                model: "requested-model".into(),
                server_fallback_regular_available_models: Some(vec!["fallback-model".into()]),
                ..OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        assert_eq!(
            executor
                .observe_server_fallback(&visible_sticky_fallback(), false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        assert!(executor.tools.is_empty());
        assert_eq!(orch.session.lock().await.model, "fallback-model");

        executor.finish_context_layers().await.unwrap();

        assert_eq!(
            orch.session.lock().await.model,
            "fallback-model",
            "the constructor snapshot is not a model layer and must not roll back an accepted visible fallback"
        );
    }

    #[tokio::test]
    async fn no_op_model_layer_carries_other_context_without_overwriting_fallback() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let call_returned = Arc::new(Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: first_started.clone(),
            release_first: release_first.clone(),
            call_returned,
            return_context_layer: true,
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig {
                model: "requested-model".into(),
                server_fallback_regular_available_models: Some(vec!["fallback-model".into()]),
                ..OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let query_history = vec![lingxi_core::types::ConversationMessage::user(
            MessageId::new(),
            "query before fallback".into(),
        )];
        let mut executor = StreamingToolExecutor::try_new(&orch, query_history.clone())
            .await
            .unwrap();
        assert_eq!(
            executor
                .observe_server_fallback(&visible_sticky_fallback(), false)
                .await
                .unwrap(),
            crate::server_fallback::ServerFallbackAdmission::Applied
        );
        assert_eq!(orch.session.lock().await.model, "fallback-model");

        let assistant_id = MessageId::new();
        for (index, held) in [true, false].into_iter().enumerate() {
            let id = ToolUseId::new();
            let input = json!({ "hold": held, "layered_model": "requested-model" });
            executor
                .add_tool_with_context_owned(
                    id.clone(),
                    "W1Capture".into(),
                    input.clone(),
                    None,
                    assistant_id,
                    crate::turn_loop::ToolUseDispatchFacts {
                        query_history: query_history.clone(),
                        assistant_message: ConversationMessage::Assistant { per_turn_effort: None,
                            id: assistant_id,
                            content: vec![ContentBlock::ToolUse { input_projection: None,
                                id,
                                name: "W1Capture".into(),
                                input,
                                provider_id: None,
                            }],
                            stop_reason: None,
                        },
                        same_turn_tool_uses: Vec::new(),
                    },
                )
                .await
                .unwrap();
            if held {
                first_started.notified().await;
                release_first.notify_one();
            }
            assert_eq!(executor.drain_one().await, Some(index));
            assert_eq!(executor.take_newly_completed().len(), 1);
        }
        executor.finish_context_layers().await.unwrap();

        let final_model = orch.session.lock().await.model.clone();
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].main_loop_model, "requested-model");
        assert_eq!(captured[1].main_loop_model, "requested-model");
        assert!(!captured[0].verbose);
        assert!(
            captured[1].verbose,
            "non-model context still carries forward"
        );
        assert_eq!(
            final_model, "fallback-model",
            "an effective no-op model modifier cannot project the old constructor model"
        );
    }

    #[tokio::test]
    async fn dropping_reset_executor_detaches_noncooperative_w1_from_rebuilt_run() {
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (dropped, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_old_tx, release_old_rx) = tokio::sync::oneshot::channel();
        let (continue_old_tx, continue_old_rx) = tokio::sync::oneshot::channel();
        let (continued, mut continued_rx) = tokio::sync::mpsc::unbounded_channel();
        let (finish_old_tx, finish_old_rx) = tokio::sync::oneshot::channel();
        let release_old = Arc::new(Mutex::new(Some(release_old_rx)));
        let continue_old = Arc::new(Mutex::new(Some(continue_old_rx)));
        let finish_old = Arc::new(Mutex::new(Some(finish_old_rx)));
        let observed_models = Arc::new(Mutex::new(Vec::new()));
        let modifier_applications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = Arc::new(MockOutputStream::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(DetachedContinuationTool {
            started,
            dropped,
            release_old,
            continue_old,
            continued,
            finish_old,
            observed_models: observed_models.clone(),
            modifier_applications: modifier_applications.clone(),
        }));
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig {
                model: "reset-base-model".into(),
                ..OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            sink.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let orch = ConversationOrchestrator::into_shared(with_context_layer_model_routes(
            orch,
            &["old-generation-layer", "new-generation-layer"],
        ));
        orch.set_tool_frame_buffering(true).await;
        let mut old_executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let old_id = ToolUseId::new();
        old_executor
            .add_tool(
                old_id.clone(),
                "DetachedContinuation".into(),
                json!({"step":"old", "safe":false}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(started_rx.recv().await.as_deref(), Some("old"));

        let removal = old_executor
            .reset_after_server_fallback_owned(None)
            .await
            .unwrap();
        assert_eq!(removal.ids, vec![old_id.clone()]);
        let actor_terminated = old_executor.scheduler.actor_termination_notifier();
        drop(old_executor);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            actor_terminated.notified(),
        )
        .await
        .expect("old actor exits after detaching its JoinSet tasks");
        // Match driver cleanup: turn buffering off before the detached W1 resumes,
        // so a stale late completion would publish directly to the live sink.
        orch.set_tool_frame_buffering(false).await;
        let history_before_late_return = orch.session.lock().await.history.len();
        release_old_tx
            .send(())
            .expect("old non-cooperative W1 call remains parked after actor exit");
        continue_old_tx
            .send(())
            .expect("detached W1 call remains resumable after actor exit");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), continued_rx.recv())
                .await
                .expect("detached W1 task reaches its continuation gate")
                .as_deref(),
            Some("old")
        );
        assert!(dropped_rx.try_recv().is_err());

        let mut rebuilt_executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let new_id = ToolUseId::new();
        rebuilt_executor
            .add_tool(
                new_id.clone(),
                "DetachedContinuation".into(),
                json!({"step":"new", "safe":true}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(started_rx.recv().await.as_deref(), Some("new"));
        assert_eq!(rebuilt_executor.drain_one().await, Some(0));
        let results = rebuilt_executor.take_newly_completed();
        assert_eq!(results.len(), 1);
        assert!(matches!(
            &results[0].block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &new_id
        ));
        rebuilt_executor.finish_context_layers().await.unwrap();
        assert_eq!(
            modifier_applications.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            orch.session.lock().await.model,
            "new-generation-layer",
            "only the rebuilt executor's layer is published"
        );

        // Now let the old call return. Its detached task can unwind, but its
        // outcome and one-shot modifier have no actor that can publish them.
        finish_old_tx
            .send(())
            .expect("old detached W1 call remains at its final gate");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx.recv())
                .await
                .expect("old W1 future eventually unwinds")
                .as_deref(),
            Some("old")
        );
        let events = sink.snapshot().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ToolCall { id, .. } if id == &old_id
            )),
            "the original ToolUseStarted remains visible"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ToolResult { id, .. } if id == &old_id
            )),
            "a detached generation cannot publish a late ToolResult"
        );
        assert!(
            !orch
                .transcript
                .tool_use_results
                .lock()
                .await
                .contains_key(old_id.as_str())
        );
        assert!(
            !orch
                .transcript
                .tool_use_mcp_meta
                .lock()
                .await
                .contains_key(old_id.as_str())
        );
        assert!(
            !orch
                .transcript
                .pending_tool_result_turn_end
                .lock()
                .await
                .contains_key(old_id.as_str())
        );
        assert_eq!(
            orch.session.lock().await.history.len(),
            history_before_late_return,
            "a detached late result cannot append transcript history"
        );
        assert_eq!(
            modifier_applications.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(rebuilt_executor.tools.len(), 1);
        assert_eq!(rebuilt_executor.tools[0].id, new_id);
        assert_eq!(orch.session.lock().await.model, "new-generation-layer");
    }

    #[tokio::test]
    async fn dropping_executor_without_reset_fences_late_w1_publication() {
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (dropped, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_old_tx, release_old_rx) = tokio::sync::oneshot::channel();
        let (continue_old_tx, continue_old_rx) = tokio::sync::oneshot::channel();
        let (continued, mut continued_rx) = tokio::sync::mpsc::unbounded_channel();
        let (finish_old_tx, finish_old_rx) = tokio::sync::oneshot::channel();
        let release_old = Arc::new(Mutex::new(Some(release_old_rx)));
        let continue_old = Arc::new(Mutex::new(Some(continue_old_rx)));
        let finish_old = Arc::new(Mutex::new(Some(finish_old_rx)));
        let observed_models = Arc::new(Mutex::new(Vec::new()));
        let modifier_applications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = Arc::new(MockOutputStream::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(DetachedContinuationTool {
            started,
            dropped,
            release_old,
            continue_old,
            continued,
            finish_old,
            observed_models,
            modifier_applications: modifier_applications.clone(),
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            sink.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        orch.set_tool_frame_buffering(true).await;
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let tool_id = ToolUseId::new();
        let dispatch_finished = executor
            .add_tool_observed_dispatch(
                tool_id.clone(),
                "DetachedContinuation".into(),
                json!({"step":"old", "safe":false}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(started_rx.recv().await.as_deref(), Some("old"));
        let actor_terminated = executor.scheduler.actor_termination_notifier();

        // No reset occurs here: executor Drop itself must invalidate the owner
        // generation while leaving the non-cooperative Tool::call alive.
        drop(executor);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            actor_terminated.notified(),
        )
        .await
        .expect("executor Drop shuts down the actor promptly");
        orch.set_tool_frame_buffering(false).await;
        let history_before_late_return = orch.session.lock().await.history.len();
        release_old_tx
            .send(())
            .expect("the non-cooperative old W1 remains parked after Drop");
        continue_old_tx
            .send(())
            .expect("the old W1 can continue after its actor has exited");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), continued_rx.recv(),)
                .await
                .expect("old W1 reaches its final gate")
                .as_deref(),
            Some("old")
        );
        finish_old_tx
            .send(())
            .expect("old W1 remains alive until explicitly released");
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx.recv())
            .await
            .expect("old Tool::call returns after the final gate");
        tokio::time::timeout(std::time::Duration::from_secs(2), dispatch_finished)
            .await
            .expect("the detached W1 dispatch eventually settles")
            .expect("the detached dispatch acknowledges completion");

        let events = sink.snapshot().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ToolCall { id, .. } if id == &tool_id
            )),
            "the initial ToolUseStarted event remains visible"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ToolResult { id, .. } if id == &tool_id
            )),
            "a Drop-only old generation cannot publish ToolResult"
        );
        assert!(
            !orch
                .transcript
                .tool_use_results
                .lock()
                .await
                .contains_key(tool_id.as_str())
        );
        assert!(
            !orch
                .transcript
                .tool_use_mcp_meta
                .lock()
                .await
                .contains_key(tool_id.as_str())
        );
        assert!(
            !orch
                .transcript
                .pending_tool_result_turn_end
                .lock()
                .await
                .contains_key(tool_id.as_str())
        );
        assert_eq!(
            orch.session.lock().await.history.len(),
            history_before_late_return,
            "a late W1 cannot append result history after Drop"
        );
        assert_eq!(
            modifier_applications.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a late one-shot context modifier cannot apply without an actor"
        );
    }

    #[tokio::test]
    async fn w1_clock_after_status_stays_bound_to_reset_generation() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("clock-after-generation.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              let releaseStaleCallback;
              let staleStateGetCatchCount = 0;
              let staleRawOutputAttempts = 0;
              let staleCallbackFinished = false;
              let liveTimerFired = false;
              on('tool.call', ($, event, next) => {
                if (event.tool === 'SafeTool' && event.scheduleStale) {
                  $.clock.after(0, async () => {
                    let release;
                    const gate = new Promise(resolve => { release = resolve; });
                    releaseStaleCallback = release;
                    $.ui.status('held stale callback entered');
                    await gate;
                    try {
                      await $.state.get({ plugin: 'clock-after-generation', key: 'stale' });
                    } catch (error) {
                      if (String(error).includes('originating Mod dispatch generation ended')) {
                        staleStateGetCatchCount += 1;
                      }
                    }
                    for (const attempt of [
                      () => $.ui.status('obsolete timer status'),
                      () => $.ui.toast('obsolete timer toast'),
                      () => $.ui.log('obsolete timer log'),
                    ]) {
                      staleRawOutputAttempts += 1;
                      attempt();
                    }
                    // ui.status/toast/log are void APIs. This second rejected
                    // API call is a worker-to-host FIFO barrier after all three
                    // raw output messages have been handled.
                    try {
                      await $.state.get({ plugin: 'clock-after-generation', key: 'stale' });
                    } catch (error) {
                      if (String(error).includes('originating Mod dispatch generation ended')) {
                        staleStateGetCatchCount += 1;
                      }
                    }
                    staleCallbackFinished = true;
                  });
                } else if (event.tool === 'SafeTool' && event.releaseStaleAndScheduleLive) {
                  if (typeof releaseStaleCallback !== 'function') {
                    throw new Error('the stale callback did not enter its release gate');
                  }
                  releaseStaleCallback();
                  $.clock.after(0, async () => {
                    $.ui.status('accepted timer status');
                    $.ui.toast('accepted timer toast');
                    $.ui.log('accepted timer log');
                    liveTimerFired = true;
                  });
                } else if (event.tool === 'SafeTool' && event.observe) {
                  if (staleStateGetCatchCount === 2 && staleRawOutputAttempts === 3
                      && staleCallbackFinished) {
                    $.ui.status('stale callback rejection and all raw output attempts reached');
                  }
                  if (liveTimerFired) $.ui.status('live timer callback ran');
                }
                return next(event);
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("clock-after-generation", dir.path(), &module, json!({}))
            .await
            .unwrap();
        let mut hook_registry = hooks::HookRegistry::new();
        hook_registry.set_mod_host(host.clone());
        let hook_registry = Arc::new(tokio::sync::RwLock::new(hook_registry));
        let sink = Arc::new(MockOutputStream::new());
        let mut tool_registry = ToolRegistry::new();
        tool_registry.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                sink.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_hook_registry(hook_registry.clone()),
        );
        let fallback_sink = Arc::new(MockOutputStream::new());
        let fallback_orch = background_context_orch(fallback_sink.clone());
        let session: Arc<dyn hooks::mods::ModSessionContext> = fallback_orch.clone();
        hook_registry
            .write()
            .await
            .attach_mod_background_context(Arc::downgrade(&session));

        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let tool_id = ToolUseId::new();
        executor
            .add_tool(
                tool_id.clone(),
                "SafeTool".into(),
                json!({"scheduleStale": true}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(executor.drain_one().await, Some(0));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let entered = sink.snapshot().await.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-generation" && text == "held stale callback entered"
                ));
                if entered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("clock.after callback entered and is held at its private Promise gate");

        let removal = executor
            .reset_after_server_fallback_owned(None)
            .await
            .unwrap();
        assert_eq!(removal.ids, vec![tool_id.clone()]);
        drop(executor);
        let mut observer = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        observer
            .add_tool(
                ToolUseId::new(),
                "SafeTool".into(),
                json!({"releaseStaleAndScheduleLive": true}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(observer.drain_one().await, Some(0));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let expected_index = observer.tools.len();
                observer
                    .add_tool(
                        ToolUseId::new(),
                        "SafeTool".into(),
                        json!({"observe": true}),
                        None,
                        MessageId::new(),
                    )
                    .await;
                assert_eq!(observer.drain_one().await, Some(expected_index));
                let events = sink.snapshot().await;
                let stale_callback_reported = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-generation"
                            && text == "stale callback rejection and all raw output attempts reached"
                ));
                let live_callback_ran = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-generation" && text == "live timer callback ran"
                ));
                let accepted_status = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-generation" && text == "accepted timer status"
                ));
                let accepted_toast = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModToast { plugin, text, .. }
                        if plugin == "clock-after-generation" && text == "accepted timer toast"
                ));
                let accepted_log = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModLog { plugin, text }
                        if plugin == "clock-after-generation" && text == "accepted timer log"
                ));
                if stale_callback_reported && live_callback_ran
                    && accepted_status && accepted_toast && accepted_log
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the held stale callback is rejected while the fresh clock callback succeeds");

        let events = sink.snapshot().await;
        let fallback_events = fallback_sink.snapshot().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ToolCall { id, .. } if id == &tool_id
            )),
            "the initial ToolUseStarted remains visible"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                    if plugin == "clock-after-generation" && text == "held stale callback entered"
            )),
            "the old callback's entered status proves it reached the non-cooperative gate"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                    if plugin == "clock-after-generation" && text == "obsolete timer status"
            )),
            "an expired generation cannot publish a raw id-zero status"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModToast { plugin, text, .. }
                    if plugin == "clock-after-generation" && text == "obsolete timer toast"
            )),
            "an expired generation cannot publish a raw id-zero toast"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModLog { plugin, text }
                    if plugin == "clock-after-generation" && text == "obsolete timer log"
            )),
            "an expired generation cannot publish a raw id-zero log"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                    if plugin == "clock-after-generation"
                        && text == "stale callback rejection and all raw output attempts reached"
            )),
            "two generation-fenced state.get catches bracketed all three old output attempts"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                if plugin == "clock-after-generation" && text == "accepted timer status"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModToast { plugin, text, .. }
                if plugin == "clock-after-generation" && text == "accepted timer toast"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModLog { plugin, text }
                if plugin == "clock-after-generation" && text == "accepted timer log"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                if plugin == "clock-after-generation" && text == "live timer callback ran"
        )));
        assert!(
            fallback_events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, .. }
                    | lingxi_core::host::OutputEvent::ModToast { plugin, .. }
                    | lingxi_core::host::OutputEvent::ModLog { plugin, .. }
                    if plugin == "clock-after-generation"
            )),
            "id-zero callback output must use its generation session, not background_context"
        );
    }

    #[tokio::test]
    async fn w1_clock_after_status_stays_bound_after_owner_drop() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("clock-after-owner-drop.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              let releaseStaleCallback;
              let staleStateGetCatchCount = 0;
              let staleRawOutputAttempts = 0;
              let staleCallbackFinished = false;
              on('tool.call', ($, event, next) => {
                if (event.tool === 'SafeTool' && event.scheduleStale) {
                  $.clock.after(0, async () => {
                    let release;
                    const gate = new Promise(resolve => { release = resolve; });
                    releaseStaleCallback = release;
                    $.ui.status('held dropped callback entered');
                    await gate;
                    try {
                      await $.state.get({ plugin: 'clock-after-owner-drop', key: 'stale' });
                    } catch (error) {
                      if (String(error).includes('originating Mod dispatch generation ended')) {
                        staleStateGetCatchCount += 1;
                      }
                    }
                    for (const attempt of [
                      () => $.ui.status('obsolete dropped timer status'),
                      () => $.ui.toast('obsolete dropped timer toast'),
                      () => $.ui.log('obsolete dropped timer log'),
                    ]) {
                      staleRawOutputAttempts += 1;
                      attempt();
                    }
                    // ui.status/toast/log are void APIs. This second rejected
                    // API call is a worker-to-host FIFO barrier after all three
                    // raw output messages have been handled.
                    try {
                      await $.state.get({ plugin: 'clock-after-owner-drop', key: 'stale' });
                    } catch (error) {
                      if (String(error).includes('originating Mod dispatch generation ended')) {
                        staleStateGetCatchCount += 1;
                      }
                    }
                    staleCallbackFinished = true;
                  });
                } else if (event.tool === 'SafeTool' && event.releaseStale) {
                  if (typeof releaseStaleCallback !== 'function') {
                    throw new Error('the dropped callback did not enter its release gate');
                  }
                  releaseStaleCallback();
                } else if (event.tool === 'SafeTool' && event.observeStale
                    && staleStateGetCatchCount === 2 && staleRawOutputAttempts === 3
                    && staleCallbackFinished) {
                  $.ui.status('dropped callback rejection and all raw output attempts reached');
                }
                return next(event);
              });
            }"#,
        )
        .unwrap();
        let live_module = dir.path().join("clock-after-owner-drop-live.js");
        std::fs::write(
            &live_module,
            r#"export function register(on) {
              let timerFired = false;
              on('tool.call', ($, event, next) => {
                if (event.tool === 'SafeTool' && event.scheduleLive) {
                  $.clock.after(0, async () => {
                    $.ui.status('accepted dropped test status');
                    $.ui.toast('accepted dropped test toast');
                    $.ui.log('accepted dropped test log');
                    timerFired = true;
                  });
                } else if (event.tool === 'SafeTool' && event.observeLive && timerFired) {
                  $.ui.status('drop test live timer callback ran');
                }
                return next(event);
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("clock-after-owner-drop", dir.path(), &module, json!({}))
            .await
            .unwrap();
        host.load(
            "clock-after-owner-drop-live",
            dir.path(),
            &live_module,
            json!({}),
        )
        .await
        .unwrap();
        let mut hook_registry = hooks::HookRegistry::new();
        hook_registry.set_mod_host(host.clone());
        let hook_registry = Arc::new(tokio::sync::RwLock::new(hook_registry));
        let sink = Arc::new(MockOutputStream::new());
        let mut tool_registry = ToolRegistry::new();
        tool_registry.register_builtin(Arc::new(SafeTool::default()) as Arc<dyn Tool>);
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                sink.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_hook_registry(hook_registry.clone()),
        );
        let fallback_sink = Arc::new(MockOutputStream::new());
        let fallback_orch = background_context_orch(fallback_sink.clone());
        let session: Arc<dyn hooks::mods::ModSessionContext> = fallback_orch.clone();
        hook_registry
            .write()
            .await
            .attach_mod_background_context(Arc::downgrade(&session));

        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let tool_id = ToolUseId::new();
        executor
            .add_tool(
                tool_id.clone(),
                "SafeTool".into(),
                json!({"scheduleStale": true}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(executor.drain_one().await, Some(0));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let entered = sink.snapshot().await.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-owner-drop" && text == "held dropped callback entered"
                ));
                if entered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("clock.after callback entered and is held at its private Promise gate");
        drop(executor);

        let mut observer = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        observer
            .add_tool(
                ToolUseId::new(),
                "SafeTool".into(),
                json!({"releaseStale": true, "scheduleLive": true}),
                None,
                MessageId::new(),
            )
            .await;
        assert_eq!(observer.drain_one().await, Some(0));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let expected_index = observer.tools.len();
                observer
                    .add_tool(
                        ToolUseId::new(),
                        "SafeTool".into(),
                        json!({"observeStale": true, "observeLive": true}),
                        None,
                        MessageId::new(),
                    )
                    .await;
                assert_eq!(observer.drain_one().await, Some(expected_index));
                let events = sink.snapshot().await;
                let stale_callback_reported = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-owner-drop"
                            && text == "dropped callback rejection and all raw output attempts reached"
                ));
                let live_callback_ran = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-owner-drop-live"
                            && text == "drop test live timer callback ran"
                ));
                let accepted_status = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                        if plugin == "clock-after-owner-drop-live"
                            && text == "accepted dropped test status"
                ));
                let accepted_toast = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModToast { plugin, text, .. }
                        if plugin == "clock-after-owner-drop-live"
                            && text == "accepted dropped test toast"
                ));
                let accepted_log = events.iter().any(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::ModLog { plugin, text }
                        if plugin == "clock-after-owner-drop-live"
                            && text == "accepted dropped test log"
                ));
                if stale_callback_reported && live_callback_ran
                    && accepted_status && accepted_toast && accepted_log
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the dropped stale callback is rejected while the fresh clock callback succeeds");

        let events = sink.snapshot().await;
        let fallback_events = fallback_sink.snapshot().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ToolCall { id, .. } if id == &tool_id
            )),
            "the initial ToolUseStarted remains visible"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                    if plugin == "clock-after-owner-drop" && text == "held dropped callback entered"
            )),
            "the old callback's entered status proves it reached the non-cooperative gate"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                    if plugin == "clock-after-owner-drop" && text == "obsolete dropped timer status"
            )),
            "a dropped generation cannot publish a raw id-zero status"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModToast { plugin, text, .. }
                    if plugin == "clock-after-owner-drop" && text == "obsolete dropped timer toast"
            )),
            "a dropped generation cannot publish a raw id-zero toast"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModLog { plugin, text }
                    if plugin == "clock-after-owner-drop" && text == "obsolete dropped timer log"
            )),
            "a dropped generation cannot publish a raw id-zero log"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                    if plugin == "clock-after-owner-drop"
                        && text == "dropped callback rejection and all raw output attempts reached"
            )),
            "two generation-fenced state.get catches bracketed all three old output attempts"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                if plugin == "clock-after-owner-drop-live" && text == "accepted dropped test status"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModToast { plugin, text, .. }
                if plugin == "clock-after-owner-drop-live" && text == "accepted dropped test toast"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModLog { plugin, text }
                if plugin == "clock-after-owner-drop-live" && text == "accepted dropped test log"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::ModStatus { plugin, text: Some(text) }
                if plugin == "clock-after-owner-drop-live"
                    && text == "drop test live timer callback ran"
        )));
        assert!(
            fallback_events.iter().all(|event| !matches!(
                event,
                lingxi_core::host::OutputEvent::ModStatus { plugin, .. }
                    | lingxi_core::host::OutputEvent::ModToast { plugin, .. }
                    | lingxi_core::host::OutputEvent::ModLog { plugin, .. }
                    if plugin == "clock-after-owner-drop" || plugin == "clock-after-owner-drop-live"
            )),
            "id-zero callback output must use its generation session, not background_context"
        );
    }
    #[tokio::test]
    async fn drop_cancels_held_progress_publication_and_unblocks_bounded_sender() {
        let fixture = progress_flood_fixture();
        fixture.orch.set_tool_frame_buffering(true).await;
        let mut executor = StreamingToolExecutor::try_new(&fixture.orch, Vec::new())
            .await
            .unwrap();
        let tool_id = ToolUseId::new();
        let dispatch_finished = executor
            .add_tool_observed_dispatch(
                tool_id.clone(),
                "ProgressFlood".into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.started.notified(),
        )
        .await
        .expect("real Tool::call entry is observed");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.sink.entered.notified(),
        )
        .await
        .expect("the progress sink is holding a live publication lease");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.queue_filled.notified(),
        )
        .await
        .expect("the bounded progress channel reaches capacity");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.blocked_send_started.notified(),
        )
        .await
        .expect("the tool blocks on the next bounded progress send");
        let actor_terminated = executor.scheduler.actor_termination_notifier();

        drop(executor);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            actor_terminated.notified(),
        )
        .await
        .expect("Drop does not wait for a backpressured output sink");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !fixture
                .sink
                .future_dropped
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("root cancellation drops the held sink future and releases its lease");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.blocked_send_finished.notified(),
        )
        .await
        .expect("the cancelled progress publisher drains enough to unblock Tool::call");
        fixture.orch.set_tool_frame_buffering(false).await;
        let history_before_late_return = fixture.orch.session.lock().await.history.len();
        fixture
            .release
            .send(())
            .expect("the still-running old-generation Tool::call remains gated");
        tokio::time::timeout(std::time::Duration::from_secs(2), dispatch_finished)
            .await
            .expect("the detached call can unwind after its release gate")
            .expect("the detached dispatch acknowledges completion");

        assert_eq!(
            fixture
                .sink
                .tool_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "initial ToolUseStarted remains visible"
        );
        assert_eq!(
            fixture
                .sink
                .tool_results
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Drop cannot publish a late ToolResult"
        );
        assert!(
            !fixture
                .orch
                .transcript
                .tool_use_results
                .lock()
                .await
                .contains_key(tool_id.as_str())
        );
        assert_eq!(
            fixture.orch.session.lock().await.history.len(),
            history_before_late_return
        );
    }

    #[tokio::test]
    async fn reset_cancels_held_progress_before_waiting_for_its_lease() {
        let fixture = progress_flood_fixture();
        fixture.orch.set_tool_frame_buffering(true).await;
        let mut executor = StreamingToolExecutor::try_new(&fixture.orch, Vec::new())
            .await
            .unwrap();
        let tool_id = ToolUseId::new();
        let dispatch_finished = executor
            .add_tool_observed_dispatch(
                tool_id.clone(),
                "ProgressFlood".into(),
                json!({}),
                None,
                MessageId::new(),
            )
            .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.started.notified(),
        )
        .await
        .expect("real Tool::call entry is observed");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.sink.entered.notified(),
        )
        .await
        .expect("the progress sink is holding a live publication lease");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.blocked_send_started.notified(),
        )
        .await
        .expect("the tool blocks on the next bounded progress send");

        let removal = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            executor.reset_after_server_fallback_owned(None),
        )
        .await
        .expect("reset cancels W1 before awaiting the progress publication lock")
        .expect("the actor remains usable after a generation reset");
        assert_eq!(removal.ids, vec![tool_id.clone()]);
        assert!(executor.tools.is_empty());
        assert!(
            fixture
                .sink
                .future_dropped
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.blocked_send_finished.notified(),
        )
        .await
        .expect("reset drains progress after dropping the blocked sink future");
        fixture.orch.set_tool_frame_buffering(false).await;
        let history_before_late_return = fixture.orch.session.lock().await.history.len();
        fixture
            .release
            .send(())
            .expect("the reset old-generation Tool::call remains alive until released");
        tokio::time::timeout(std::time::Duration::from_secs(2), dispatch_finished)
            .await
            .expect("the old dispatch completes after its explicit release")
            .expect("the detached dispatch acknowledges completion");

        assert_eq!(
            fixture
                .sink
                .tool_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "initial ToolUseStarted remains visible"
        );
        assert_eq!(
            fixture
                .sink
                .tool_results
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "reset fences the old result frame"
        );
        assert!(
            !fixture
                .orch
                .transcript
                .tool_use_results
                .lock()
                .await
                .contains_key(tool_id.as_str())
        );
        assert_eq!(
            fixture.orch.session.lock().await.history.len(),
            history_before_late_return
        );
        let actor_terminated = executor.scheduler.actor_termination_notifier();
        drop(executor);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            actor_terminated.notified(),
        )
        .await
        .expect("test cleanup shuts down the reset actor");
    }

    #[tokio::test]
    async fn completed_w1_autonomously_starts_queued_unsafe_sibling_before_tn() {
        let _env_guard = tool_concurrency_env_guard();
        std::env::set_var("LINGXI_MAX_TOOL_USE_CONCURRENCY", "2");
        struct ClearConcurrencyEnv;
        impl Drop for ClearConcurrencyEnv {
            fn drop(&mut self) {
                std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
            }
        }
        let _clear = ClearConcurrencyEnv;

        let captured = Arc::new(Mutex::new(Vec::new()));
        let queued_sibling_started = Arc::new(Notify::new());
        let release_queued_sibling = Arc::new(Notify::new());
        let first_call_returned = Arc::new(Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(W1ContextRecordingTool {
            captured: captured.clone(),
            first_started: queued_sibling_started.clone(),
            release_first: release_queued_sibling.clone(),
            call_returned: first_call_returned.clone(),
            return_context_layer: false,
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let assistant_id = MessageId::new();
        let first_id = ToolUseId::new();
        let second_id = ToolUseId::new();
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        executor
            .add_tool(
                first_id.clone(),
                "W1Capture".into(),
                json!({"hold": false}),
                None,
                assistant_id,
            )
            .await;
        executor
            .add_tool(
                second_id.clone(),
                "W1Capture".into(),
                json!({"hold": true}),
                None,
                assistant_id,
            )
            .await;

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            first_call_returned.notified().await;
            queued_sibling_started.notified().await;
        })
        .await
        .expect("both real W1 calls enter without a provider/Tn poll");
        assert_eq!(captured.lock().unwrap().len(), 2);

        let mut ready_count = 0;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while executor.tools[0].status != ToolStatus::Completed {
                ready_count += executor.drain_ready().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bounded stage=first-completion-ready-poll");
        assert_eq!(
            ready_count, 1,
            "Tn sees A while the newly started unsafe sibling B remains executing"
        );
        let ready = executor.take_newly_completed();
        assert_eq!(ready.len(), 1);
        assert!(matches!(
            ready[0].block,
            ContentBlock::ToolResult { ref tool_use_id, .. } if tool_use_id.as_str() == first_id.as_str()
        ));
        assert_eq!(executor.tools[0].status, ToolStatus::Yielded);
        assert_eq!(executor.tools[1].status, ToolStatus::Executing);

        release_queued_sibling.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), executor.drain_one())
            .await
            .expect("bounded stage=held-sibling-drain");
    }

    #[tokio::test]
    async fn safe_dispatch_advances_while_permission_is_parked_and_reset_returns() {
        let _env_guard = tool_concurrency_env_guard();
        std::env::set_var("LINGXI_MAX_TOOL_USE_CONCURRENCY", "2");
        struct ClearConcurrencyEnv;
        impl Drop for ClearConcurrencyEnv {
            fn drop(&mut self) {
                std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
            }
        }
        let _clear = ClearConcurrencyEnv;

        let first_permission_waiting = Arc::new(Notify::new());
        let release_first_permission = Arc::new(Notify::new());
        let first_permission_finished = Arc::new(Notify::new());
        let first_saw_cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_call_started = Arc::new(Notify::new());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(ParkedPermissionTool {
            first_permission_waiting: first_permission_waiting.clone(),
            release_first_permission: release_first_permission.clone(),
            first_permission_finished: first_permission_finished.clone(),
            first_saw_cancellation: first_saw_cancellation.clone(),
            second_call_started: second_call_started.clone(),
        }));
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ));
        let assistant_id = MessageId::new();
        let first_id = ToolUseId::new();
        let second_id = ToolUseId::new();
        let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        let initial_model = orch.session.lock().await.model.clone();
        executor
            .add_tool(
                first_id.clone(),
                "ParkedPermission".into(),
                json!({ "step": "first" }),
                None,
                assistant_id,
            )
            .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            first_permission_waiting.notified(),
        )
        .await
        .expect("first safe W1 reaches and parks in check_permissions");

        executor
            .add_tool(
                second_id.clone(),
                "ParkedPermission".into(),
                json!({ "step": "second" }),
                None,
                assistant_id,
            )
            .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            second_call_started.notified(),
        )
        .await
        .expect("safe B reaches Tool::call while safe A remains in permission");

        let removal = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            executor.reset_after_server_fallback(Some(ToolUseRemovalReason::FallbackSweep)),
        )
        .await
        .expect("reset is not blocked by A's permission future");
        assert_eq!(removal.ids, vec![first_id, second_id]);
        assert!(
            !first_saw_cancellation.load(std::sync::atomic::Ordering::SeqCst),
            "permission remains parked until explicitly released"
        );
        release_first_permission.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            first_permission_finished.notified(),
        )
        .await
        .expect("released permission call completes after reset");
        assert!(first_saw_cancellation.load(std::sync::atomic::Ordering::SeqCst));
        assert!(executor.tools.is_empty(), "late old rows remain tombstoned");
        assert!(executor.take_newly_completed().is_empty());
        assert!(executor.is_current_generation_idle().await);
        executor.finish_context_layers().await.unwrap();
        assert_eq!(
            orch.session.lock().await.model,
            initial_model,
            "a late old-generation permission result cannot project stale context"
        );
    }
}
