//!
//! Asserts that both tools dispatch CONCURRENTLY (the slower one does
//! NOT delay the faster one's `OutputStream::ToolResult` emission) AND
//! that both `ToolResult` content blocks appear in the next user
//! message in the ORIGINAL (in-stream) order.

use async_trait::async_trait;
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::OutputEvent;
use protocol::ToolUseId;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

struct SlowTool;
struct FastTool;

macro_rules! impl_test_tool {
    ($ty:ty, $name:literal, $sleep_ms:expr) => {
        #[async_trait]
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn input_schema(&self) -> &serde_json::Value {
                static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                    once_cell::sync::Lazy::new(|| json!({"type": "object"}));
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
                _ctx: &tool_api::context::ToolUseContext,
            ) -> Result<(), ValidationError> {
                Ok(())
            }
            async fn check_permissions(
                &self,
                _input: &serde_json::Value,
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
                _input: &serde_json::Value,
                _opts: &DescriptionOptions,
            ) -> String {
                $name.into()
            }
            async fn prompt(&self, _opts: &PromptOptions) -> String {
                String::new()
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: tool_api::context::ToolUseContext,
                _tx: ToolProgressSender,
            ) -> Result<ToolCallResult, ToolError> {
                let sleep_ms: u64 = $sleep_ms;
                if sleep_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                Ok(ToolCallResult {
                    data: json!({"tool": $name}),
                    model_content: None,
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
        }
    };
}

impl_test_tool!(SlowTool, "Slow", 50u64);
impl_test_tool!(FastTool, "Fast", 0u64);

fn registry_with_slow_and_fast() -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(Arc::new(SlowTool));
    r.register_builtin(Arc::new(FastTool));
    Arc::new(r)
}

#[tokio::test]
async fn two_tools_dispatched_concurrently_results_ordered() {
    let id_slow = ToolUseId::new();
    let id_fast = ToolUseId::new();
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, id_slow.clone(), "Slow"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        content_block_start_tool_use(1, id_fast.clone(), "Fast"),
        input_json_delta(1, "{}"),
        content_block_stop(1),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = scripted![
        message_start("m2", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "Done."),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());

    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api.clone(),
        registry_with_slow_and_fast(),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );

    let start = std::time::Instant::now();
    let _ = orch.run_turn_streaming("call two tools").await.expect("ok");
    let elapsed = start.elapsed();

    // Tool-schema wiring (batch 18): the streaming turn advertised the
    // registry's wire tool definitions on its round-trips — serialized
    // `{name, description, input_schema}`, sorted by name.
    let calls = api.captured_calls().await;
    let first_tools: Vec<&str> = calls[0]
        .tools
        .iter()
        .map(|t| t["name"].as_str().expect("wire tool name"))
        .collect();
    assert_eq!(
        first_tools,
        vec!["Fast", "Slow"],
        "run_turn_streaming must advertise the registry's wire tools (sorted by name)"
    );

    // Concurrency is proven DETERMINISTICALLY by the completion-order assertion
    // below (`result_tools == ["Fast", "Slow"]`): Fast is dispatched SECOND but
    // emits its ToolResult FIRST, which is only possible if the two tools ran
    // concurrently — sequential dispatch-order execution would complete Slow
    // first. The wall-clock measurement here is therefore only a coarse
    // hang-guard: a generous bound that catches a genuine deadlock without
    // flaking under the CPU contention of a full `cargo test --workspace` run.
    // (The previous 90ms bound was contention-sensitive AND did not actually
    // distinguish concurrency: FastTool sleeps 0ms, so concurrent and sequential
    // wall-times are both ~50ms — the bound only measured scheduler latency.)
    assert!(
        elapsed < Duration::from_secs(5),
        "tool dispatch appears to have hung (expected well under 5s, got {elapsed:?})"
    );

    let events = output.snapshot().await;
    // This test asserts the tool-lifecycle ORDERING. The §0.7
    // "light up thinking/usage" follow-up adds additive `Usage` emits
    // (from `message_start` / `message_delta`) that interleave but are
    // orthogonal to that ordering, so filter them (and `Thinking`) out
    // before the sequence assertion. The final completing turn now carries
    // visible text ("Done.") so the #78 thinking-only nudge does not fire —
    // that `Text` event is likewise orthogonal to tool ordering, so filter it.
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolCall { .. } => Some("ToolCall"),
            OutputEvent::ToolResult { .. } => Some("ToolResult"),
            OutputEvent::EndTurn { .. } => Some("EndTurn"),
            OutputEvent::Text { .. }
            | OutputEvent::Usage { .. }
            | OutputEvent::Thinking { .. }
            | OutputEvent::MessageIdentity { .. } => None,
            // A long-running tool (the `Slow` arm) emits periodic
            // `ToolHeartbeat`s; they are timing-dependent and orthogonal to the
            // tool-lifecycle ordering under test, so filter them out too.
            OutputEvent::ToolHeartbeat { .. } => None,
            _ => Some("Other"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "ToolCall",
            "ToolCall",
            "ToolResult",
            "ToolResult",
            "EndTurn"
        ],
        "events: {events:?}"
    );

    // ToolCalls fire in dispatch order (Slow first because its
    // content_block_stop arrived first).
    let call_tools: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolCall { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(call_tools, vec!["Slow", "Fast"]);

    // ToolResults fire in COMPLETION order — Fast finishes first.
    //
    // DO NOT "correct" this to received order. It looks like a bug and is not.
    // claude-code's `getCompletedResults` (2.1.220 binary @232976312) walks the
    // tools in received order but only stops early on
    // `e.status==="executing" && !e.isConcurrencySafe` — an executing tool that
    // IS concurrency-safe falls through, so a later tool that already finished
    // gets yielded ahead of it. Both tools here are concurrency-safe, so
    // completion order is the correct parity outcome.
    //
    // The `break` arm — the one real ordering guarantee — is locked separately
    // in `streaming_frame_order_test`.
    let result_tools: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolResult { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(result_tools, vec!["Fast", "Slow"]);
}
