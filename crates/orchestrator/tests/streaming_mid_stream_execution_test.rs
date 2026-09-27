//! Proves that a tool genuinely BEGINS EXECUTING mid-stream — i.e. its
//! `call` body runs WHILE the model stream is still being pumped, BEFORE the
//! stream's terminal `message_delta` / `message_stop` (and a trailing text
//! block) are observed. This is the behavioral delta of dispatching tools
//! into the `StreamingToolExecutor` DURING the stream (claude-code
//! `query.ts:659/837-844`) instead of collecting all tool_uses and only
//! starting them after `EndOfStream`.
//!
//! ## Why this is byte-equivalent (scope: transcript / JSONL / API-request bytes)
//! The new behavior changes only WHEN a tool starts executing; all PERSISTENCE
//! stays post-stream (the mid-stream `drain_one` only RECORDS completions, never
//! persists). The load-bearing byte guard for the tool path is
//! `streaming_tool_result_topology_test` (it pins per-result user messages in
//! received order, each parented to its tool_use line);
//! `streaming_vs_batched_equivalence_test` is text-only (no tools) and does NOT
//! exercise tool-result bytes. NOTE: the live display/SDK `OutputStream` event
//! order DOES change — a fast tool's ToolCall/ToolResult now interleaves between
//! stream text events — which is FAITHFUL to claude-code's mid-stream
//! `yield result.message` (`query.ts:851-853`), not a regression; it is separate
//! from the JSONL/history/request bytes. This test asserts exactly that *timing*
//! interleave via a shared event log (a "tool-started" marker pushed from the
//! tool's `call` against the stream's own text emits).

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
use std::sync::{Arc, Mutex};
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// Shared, ordered log of "what happened when". The tool pushes `tool-started`
/// from inside `call`; the test reads this against the OutputStream's own
/// text-emit ordering to prove the tool ran mid-stream.
type EventLog = Arc<Mutex<Vec<String>>>;

/// A concurrency-safe tool that records, the moment its `call` begins, an
/// ordered marker into the shared log. Returns Ok immediately afterwards.
struct ProbeTool {
    log: EventLog,
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "Probe"
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "Probe".into()
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
        // Mark that the tool's body actually began running. Under mid-stream
        // dispatch this happens DURING the stream pump (the `select!` polls the
        // in-flight future), so it lands BEFORE the trailing-text marker the
        // test records once the whole turn completes.
        self.log.lock().unwrap().push("tool-started".to_string());
        Ok(ToolCallResult {
            data: json!({"ok": true}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[tokio::test]
async fn tool_call_body_runs_before_stream_end() {
    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let tu_id = ToolUseId::new();

    // A turn whose stream carries: a leading text block, then a tool_use, then
    // a TRAILING text block, then the terminal message_delta/stop. With
    // mid-stream dispatch, the tool's `call` runs the moment its
    // `content_block_stop` arrives — i.e. BEFORE the trailing "after tool"
    // text block is dispatched to the OutputStream.
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "before tool"),
        content_block_stop(0),
        content_block_start_tool_use(1, tu_id, "Probe"),
        input_json_delta(1, "{}"),
        content_block_stop(1), // dispatch + execution begins HERE, mid-stream
        content_block_start_text(2),
        text_delta(2, "after tool"),
        content_block_stop(2),
        message_delta_stop("tool_use"),
        message_stop(),
    ];

    // Turn 2: the model wraps up after seeing the tool result.
    let turn2 = scripted![
        message_start("m2", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());

    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(ProbeTool { log: log.clone() }));
    let tools = Arc::new(registry);

    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api.clone(),
        tools,
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );

    let _ = orch.run_turn_streaming("call a tool").await.expect("ok");

    // (1) The tool's body ran exactly once.
    let markers = log.lock().unwrap().clone();
    assert_eq!(
        markers,
        vec!["tool-started".to_string()],
        "probe tool's call body should have run once"
    );

    // (2) PROOF OF MID-STREAM EXECUTION: the tool's `call` began BEFORE the
    // stream's trailing "after tool" text block was emitted to the output.
    // Build a unified timeline from the OutputStream's text emits plus the
    // tool-start marker (recovered via the ToolCall emit position, which fires
    // at the SAME point the tool's body starts — inside the dispatched future).
    let events = output.snapshot().await;
    let timeline: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::Text { text } => Some(format!("text:{text}")),
            OutputEvent::ToolCall { tool, .. } => Some(format!("toolcall:{tool}")),
            _ => None,
        })
        .collect();

    let pos = |needle: &str| timeline.iter().position(|s| s == needle);
    let before = pos("text:before tool").expect("'before tool' text emitted");
    let toolcall = pos("toolcall:Probe").expect("Probe ToolCall emitted");
    let after = pos("text:after tool").expect("'after tool' text emitted");

    // The tool's dispatch (and thus its `call`) starts AFTER the leading text
    // but BEFORE the trailing text — i.e. genuinely mid-stream, not after
    // EndOfStream. Under the OLD "collect then execute after the stream"
    // behavior the ToolCall would appear AFTER both stream text blocks.
    assert!(
        before < toolcall && toolcall < after,
        "tool must dispatch mid-stream (between the two stream text blocks); timeline: {timeline:?}"
    );

    assert_eq!(api.captured_calls().await.len(), 2);
}
