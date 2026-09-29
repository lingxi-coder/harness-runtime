//! Sub-task 11b CAPSTONE topology test: wiring the `StreamingToolExecutor` into
//! the live streaming turn flips tool-result persistence to claude-code's
//! per-result, assistant-parented shape (TS `StreamingToolExecutor` +
//! `query.ts:826-862` + `sessionStorage` `sourceToolAssistantUUID → parentUuid`).
//!
//! For a streaming assistant turn that requests ≥2 tools, this asserts:
//!   1. Each `tool_result` is its OWN `user` message in `session.history`
//!      (N parented messages — NOT one batched user message carrying all N
//!      result blocks).
//!   2. Each result's JSONL `parentUuid` == the assistant line's uuid (every
//!      result parents to the assistant that requested it).
//!   3. The N results appear in RECEIVED order (the in-stream tool order).

use async_trait::async_trait;
use lingxi_core::host::FileSystem;
use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_posix::fs::PosixFileSystem;
use serde_json::json;
use session::jsonl::reader::JsonlReader;
use session::jsonl::writer::JsonlWriter;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// A trivial concurrency-safe tool whose result echoes its own name, so the
/// per-result blocks can be matched back to received (in-stream) order.
struct EchoTool {
    name: &'static str,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        self.name
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
        self.name.into()
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
        Ok(ToolCallResult {
            data: json!({ "content": format!("result-of-{}", self.name) }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn registry_with_three() -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(Arc::new(EchoTool { name: "Alpha" }));
    r.register_builtin(Arc::new(EchoTool { name: "Bravo" }));
    r.register_builtin(Arc::new(EchoTool { name: "Charlie" }));
    Arc::new(r)
}

#[tokio::test]
async fn streaming_tool_results_are_per_result_assistant_parented() {
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    // Three tool_use blocks in one streaming assistant turn (received order
    // Alpha → Bravo → Charlie), then a second turn that ends.
    let id_a = ToolUseId::new();
    let id_b = ToolUseId::new();
    let id_c = ToolUseId::new();
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, id_a.clone(), "Alpha"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        content_block_start_tool_use(1, id_b.clone(), "Bravo"),
        input_json_delta(1, "{}"),
        content_block_stop(1),
        content_block_start_tool_use(2, id_c.clone(), "Charlie"),
        input_json_delta(2, "{}"),
        content_block_stop(2),
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

    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api,
        registry_with_three(),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
    .with_jsonl_writer(writer);

    let _ = orch
        .run_turn_streaming("call three tools")
        .await
        .expect("ok");

    // ── (1) + (3): N per-result user messages in session.history, in received order
    let session = orch.session();
    let s = session.lock().await;

    // Collect each User message that carries exactly one ToolResult block —
    // these are the per-result tool_result user messages.
    let tool_result_msgs: Vec<&Vec<ContentBlock>> = s
        .history
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::User { content, .. }
                if content.len() == 1 && matches!(content[0], ContentBlock::ToolResult { .. }) =>
            {
                Some(content)
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        tool_result_msgs.len(),
        3,
        "expected 3 separate tool_result user messages (per-result topology), \
         got {} — history: {:?}",
        tool_result_msgs.len(),
        s.history
    );

    // No single batched user message should carry more than one tool_result.
    assert!(
        !s.history.iter().any(|m| matches!(m,
            ConversationMessage::User { content, .. }
                if content.iter().filter(|b| matches!(b, ContentBlock::ToolResult { .. })).count() > 1)),
        "no user message may batch multiple tool_result blocks (per-result topology): {:?}",
        s.history
    );

    // Received order: Alpha, Bravo, Charlie (by tool_use_id).
    let result_ids: Vec<ToolUseId> = tool_result_msgs
        .iter()
        .map(|content| match &content[0] {
            ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.clone(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        result_ids,
        vec![id_a, id_b, id_c],
        "tool_result user messages must appear in RECEIVED (in-stream) order"
    );
    drop(s);

    // ── (2): each result's JSONL parentUuid == ITS tool_use's per-block line uuid
    //
    // WRITE-side per-block split (claude.ts:2171-2211): the streaming turn now
    // emits ONE single-block assistant JSONL line per content block — three
    // tool_use blocks → THREE assistant lines, each carrying ONE tool_use, all
    // sharing the same inner `message.id` but with DISTINCT top-level uuids. Each
    // tool_result parents to ITS OWN tool_use line's uuid (sessionStorage.ts:1028
    // `sourceToolAssistantUUID`), NOT one shared per-turn assistant parent.
    let reader = JsonlReader::new(session_path, fs);
    let msgs = reader.read_all().await.expect("read_all");

    // The three per-block assistant lines (one tool_use each), all sharing the
    // same inner message.id. The final completing turn carries visible text
    // ("Done.") so the #78 thinking-only nudge does not fire — that pure-text
    // assistant line is a separate turn (distinct inner message.id) and is not
    // part of the tool-use topology, so scope to the tool_use-bearing lines.
    let assistant_lines: Vec<_> = msgs
        .iter()
        .filter(|m| m.message_type == "assistant")
        .filter(|m| {
            m.message
                .get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                })
        })
        .collect();
    assert_eq!(
        assistant_lines.len(),
        3,
        "expected 3 single-block assistant lines (one per tool_use), got {}",
        assistant_lines.len()
    );
    // All share one inner message.id (distinct top-level uuids).
    let inner_ids: std::collections::HashSet<&str> = assistant_lines
        .iter()
        .map(|m| {
            m.message
                .get("id")
                .and_then(|v| v.as_str())
                .expect("inner id")
        })
        .collect();
    assert_eq!(inner_ids.len(), 1, "all blocks share one inner message.id");
    let top_uuids: std::collections::HashSet<&str> =
        assistant_lines.iter().map(|m| m.uuid.as_str()).collect();
    assert_eq!(top_uuids.len(), 3, "three distinct top-level uuids");

    // Map each per-block assistant line's contained tool_use_id -> its line uuid.
    let mut line_uuid_by_tool_use_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in &assistant_lines {
        let blocks = m
            .message
            .get("content")
            .and_then(|c| c.as_array())
            .expect("assistant content array");
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let tid = b.get("id").and_then(|i| i.as_str()).expect("tool_use id");
                line_uuid_by_tool_use_id.insert(tid.to_string(), m.uuid.clone());
            }
        }
    }
    assert_eq!(
        line_uuid_by_tool_use_id.len(),
        3,
        "each assistant line must carry exactly one tool_use"
    );

    // Each tool_result user line must parent to ITS tool_use's line uuid.
    let tool_result_lines: Vec<_> = msgs
        .iter()
        .filter(|m| {
            m.message_type == "user"
                && serde_json::to_string(&m.message)
                    .map(|s| s.contains("tool_result"))
                    .unwrap_or(false)
        })
        .collect();

    assert_eq!(
        tool_result_lines.len(),
        3,
        "expected 3 tool_result JSONL user lines, got {}",
        tool_result_lines.len()
    );
    for line in &tool_result_lines {
        // Extract the result's tool_use_id and confirm it parents to that
        // tool_use's per-block assistant line.
        let blocks = line
            .message
            .get("content")
            .and_then(|c| c.as_array())
            .expect("tool_result content array");
        let tid = blocks
            .iter()
            .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            .and_then(|b| b.get("tool_use_id").and_then(|i| i.as_str()))
            .expect("tool_result tool_use_id");
        let expected = line_uuid_by_tool_use_id
            .get(tid)
            .unwrap_or_else(|| panic!("no per-block line for tool_use_id {tid}"));
        assert_eq!(
            line.parent_uuid.as_deref(),
            Some(expected.as_str()),
            "tool_result for {tid} must parent to ITS tool_use line uuid \
             (sourceToolAssistantUUID per-tool reparenting)"
        );
    }
    // And the three parents must be DISTINCT (no shared per-turn parent).
    let parents: std::collections::HashSet<_> = tool_result_lines
        .iter()
        .map(|m| m.parent_uuid.clone())
        .collect();
    assert_eq!(
        parents.len(),
        3,
        "the three tool_results must NOT share one parent (per-tool reparenting)"
    );
}
