//! End-to-end SDK `ToolResult` frame ordering through the streaming driver.
//!
//! The companion unit tests in `conversation.rs` (`tool_frame_ordering_tests`)
//! pin the buffer/release MECHANISM in isolation; `streaming_concurrent_tools_test`
//! covers two concurrency-SAFE tools. Neither exercises the rule that actually
//! decides frame order, which lives in claude-code's `getCompletedResults`
//! (2.1.220 binary @232976312):
//!
//! ```js
//! *getCompletedResults(){
//!   if(this.discarded)return;
//!   for(let e of this.tools){
//!     …
//!     if(e.status==="yielded")continue;
//!     if(e.status==="completed"){e.status="yielded";
//!       for(let t of e.results)yield{message:t,newContext:this.toolUseContext};…}
//!     else if(e.status==="executing"&&!e.isConcurrencySafe)break
//!   }
//! }
//! ```
//!
//! So the order is NOT simply "received" and NOT simply "completion":
//!
//! - The walk is in RECEIVED order, and a `completed` tool is yielded as soon as
//!   the walk reaches it.
//! - An `executing` tool that IS concurrency-safe does not stop the walk (no
//!   `break`, no `continue` — it falls through), so a later tool that finished
//!   first is yielded first. That is why two safe tools emit in COMPLETION
//!   order, which `streaming_concurrent_tools_test` asserts.
//! - An `executing` tool that is NOT concurrency-safe `break`s the walk, holding
//!   back every later result even one that is already `completed`.
//!
//! That last arm is the only hard ordering guarantee in the design, and it is
//! what this file locks. Making it observable needs a later tool that reaches
//! `completed` WITHOUT being dispatched — otherwise the exclusive tool's own
//! run-alone gating would have serialised things anyway and the assertion would
//! pass for the wrong reason. An unknown tool name is exactly that: the executor
//! marks it `Completed` at `add_tool` time with a validation error.

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

/// A slow, NON-concurrency-safe tool — the `executing && !isConcurrencySafe`
/// arm of the walk.
struct ExclusiveTool;

#[async_trait]
impl Tool for ExclusiveTool {
    fn name(&self) -> &str {
        "Exclusive"
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
    /// The whole point of this fixture.
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
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
        "Exclusive".into()
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
        // Long enough that an immediately-`completed` later tool would certainly
        // win any race that was not explicitly ordered.
        tokio::time::sleep(Duration::from_millis(120)).await;
        Ok(ToolCallResult {
            data: json!({"tool": "Exclusive"}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn registry_with_exclusive() -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(Arc::new(ExclusiveTool));
    Arc::new(r)
}

/// An already-`completed` later tool must WAIT behind an executing
/// non-concurrency-safe earlier tool.
///
/// `NoSuchTool` is unknown, so the executor marks it `Completed` at `add_tool`
/// time — its result is ready before `Exclusive` has run a single millisecond.
/// If the driver emitted frames as results became ready, the unknown tool's
/// error would come out FIRST. The `break` in `getCompletedResults` is what
/// stops that, and it is the guarantee under test.
#[tokio::test]
async fn a_ready_result_waits_behind_an_executing_exclusive_tool() {
    let id_exclusive = ToolUseId::new();
    let id_unknown = ToolUseId::new();
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, id_exclusive.clone(), "Exclusive"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        content_block_start_tool_use(1, id_unknown.clone(), "NoSuchTool"),
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
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        api.clone(),
        registry_with_exclusive(),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );

    orch.run_turn_streaming("call both").await.expect("ok");

    let events = output.snapshot().await;
    let result_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolResult { id, .. } => Some(id.to_string()),
            _ => None,
        })
        .collect();

    assert_eq!(
        result_ids,
        vec![id_exclusive.to_string(), id_unknown.to_string()],
        "the unknown tool's already-ready result must not overtake the executing \
         non-concurrency-safe tool (getCompletedResults `break`); events: {events:?}"
    );
}
