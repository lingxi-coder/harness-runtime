use super::{dispatch_tool_uses_tracked, ConversationOrchestrator};
use crate::test_support::{MockApiClient, MockOutputStream, NoOpPermissionGate};
use crate::tool_result_persistence::{PERSISTED_OUTPUT_OPEN, TOOL_RESULTS_DIR};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use lingxi_core::types::{ContentBlock, ToolUseId};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// Emits `input.len` bytes of `x` as its model content, plus (when
/// `input.blocks` is set) a raw `content_blocks` array. Declares a 100-byte
/// persistence threshold so the boundary is cheap to drive.
struct SizedTool;

const THRESHOLD: usize = 100;

#[async_trait]
impl Tool for SizedTool {
    fn name(&self) -> &str {
        "Sized"
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
    fn persistence_threshold(&self) -> Option<usize> {
        Some(THRESHOLD)
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "sized".into()
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
        let len = usize::try_from(
            input
                .get("len")
                .and_then(serde_json::Value::as_u64)
                .unwrap(),
        )
        .unwrap();
        let mut body = "x".repeat(len);
        if input
            .get("split_surrogate")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            body.replace_range(1999..2001, "😀");
        }
        Ok(ToolCallResult {
            data: json!(body),
            model_content: Some(body),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn orch_with(tool: Arc<dyn Tool>, config_home: Option<PathBuf>) -> ConversationOrchestrator {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(tool);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        crate::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        PathBuf::from("/tmp/wsp"),
    );
    match config_home {
        Some(h) => orch.with_config_home(h),
        None => orch,
    }
}

fn use_of(name: &str, len: usize) -> Vec<(ToolUseId, String, serde_json::Value, Option<String>)> {
    vec![(ToolUseId::new(), name.into(), json!({ "len": len }), None)]
}

async fn dispatch_content(
    orch: &ConversationOrchestrator,
    uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
) -> String {
    let (results, _p, _i, _m) = dispatch_tool_uses_tracked(orch, uses, None)
        .await
        .expect("dispatch");
    let ContentBlock::ToolResult { content, .. } = &results[0] else {
        panic!("expected ToolResult");
    };
    content.clone()
}

#[tokio::test]
async fn split_surrogate_survives_dispatch_jsonl_resume_and_request_encoding() {
    use lingxi_core::types::{ConversationMessage, MessageId};
    use llm_runtime::services::sdk::{self, WireCodec};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("history.jsonl");
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        path.clone(),
        Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.path().into())),
    ));
    let orch = orch_with(Arc::new(SizedTool), Some(tmp.path().into())).with_jsonl_writer(writer);
    let mut call = use_of("Sized", 4000).remove(0);
    call.2["split_surrogate"] = json!(true);
    let assistant = ConversationMessage::Assistant {
        id: MessageId::new(),
        stop_reason: Some("tool_use".into()),
        content: vec![ContentBlock::ToolUse {
            id: call.0.clone(),
            name: "Sized".into(),
            input: call.2.clone(),
            provider_id: None,
        }],
    };
    orch.persist_message_to_jsonl(&assistant).await;
    let (results, ..) = dispatch_tool_uses_tracked(&orch, &[call], None)
        .await
        .unwrap();
    let mut user = ConversationMessage::user(MessageId::new(), String::new());
    if let ConversationMessage::User { content, .. } = &mut user {
        *content = results;
    }
    orch.persist_message_to_jsonl(&user).await;
    let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
    let history =
        crate::resume::state_from_messages(uuid::Uuid::nil(), &loaded.messages_in_order).history;
    let messages = llm_runtime::convert::to_llm_messages(history).unwrap();
    let (input, overrides) = llm_runtime::convert::history_input(
        "claude-opus-4-7",
        &messages,
        &[],
        &[],
        sdk::protocol::ProtocolFamily::AnthropicMessages,
    )
    .unwrap();
    let profile: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
        "provider_id":"anthropic", "profile_name":"test", "base_url":"https://api.anthropic.com",
        "protocol":"anthropic_messages", "auth":"none", "models":[]
    }))
    .unwrap();
    let codec = sdk::AnthropicMessagesCodec;
    for mode in [sdk::RequestMode::Complete, sdk::RequestMode::CountTokens] {
        let encoded = codec
            .encode_request(
                sdk::EncodeRequest::new(&input),
                &sdk::CodecContext::new(&profile, &input.model, mode),
            )
            .unwrap();
        let body = serde_json::from_slice(&encoded.body).unwrap();
        let wire =
            String::from_utf8(sdk::exact_json::serialize(&body, &overrides).unwrap()).unwrap();
        assert!(
            wire.contains("\\ud83d\\n..."),
            "exact JS surrogate must reach wire: {wire}"
        );
        assert!(
            !wire.contains("lingxi_tool_result_string_utf16"),
            "sidecar must not leak"
        );
        assert!(
            !wire.contains('\u{fffd}'),
            "display replacement must not reach Claude"
        );
    }
}

/// T5 — `o<=i` returns the result UNCHANGED; only a STRICTLY larger body
/// is persisted.
#[tokio::test]
async fn exactly_at_the_threshold_is_not_persisted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let orch = orch_with(Arc::new(SizedTool), Some(tmp.path().to_path_buf()));
    let content = dispatch_content(&orch, &use_of("Sized", THRESHOLD)).await;
    assert_eq!(content, "x".repeat(THRESHOLD));
}

#[tokio::test]
async fn one_byte_over_the_threshold_is_persisted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let orch = orch_with(Arc::new(SizedTool), Some(tmp.path().to_path_buf()));
    let content = dispatch_content(&orch, &use_of("Sized", THRESHOLD + 1)).await;
    assert!(
        content.starts_with(PERSISTED_OUTPUT_OPEN),
        "expected the persisted envelope, got: {content}"
    );
    assert!(content.contains("Output too large (101 bytes)."));
}

/// T8 — the file lands at
/// `<config_home>/projects/<project_dir_name(cwd)>/<uuid>/tool-results/<id>.txt`.
#[tokio::test]
async fn persisted_file_is_session_scoped_and_holds_the_full_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let orch = orch_with(Arc::new(SizedTool), Some(tmp.path().to_path_buf()));
    let uses = use_of("Sized", 5_000);
    let content = dispatch_content(&orch, &uses).await;
    let session_uuid = {
        let s = orch.session.lock().await;
        s.session_id.as_uuid().to_string()
    };
    let dir = tmp
        .path()
        .join("projects")
        .join(session::jsonl::path::project_dir_name(
            &orch.current_cwd().to_string_lossy(),
        ))
        .join(session_uuid)
        .join(TOOL_RESULTS_DIR);
    let file = dir.join(format!("{}.txt", uses[0].0.as_str()));
    assert!(
        file.exists(),
        "expected {} to exist; envelope was: {content}",
        file.display()
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("read back").len(),
        5_000
    );
    assert!(content.contains(&file.display().to_string()));
}

/// A process runner has already persisted the full body under its rooted
/// task path, so the orchestrator must point the model at that file rather
/// than creating a second tool-use-id file with duplicate bytes.
#[tokio::test]
async fn process_output_persistence_reuses_task_file_without_duplicate_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let orch = orch_with(Arc::new(SizedTool), Some(tmp.path().to_path_buf()));
    let output_path = tmp.path().join("tasks/local_bash_spilled.out");
    std::fs::create_dir_all(output_path.parent().expect("task dir")).expect("task dir");
    std::fs::write(&output_path, "x".repeat(5_000)).expect("task output");
    let data = json!({
        "persistedOutputPath": output_path,
        "persistedOutputSize": 5_000,
    });
    let output_file = super::process_output_file_from_data(&data).expect("all metadata");
    assert_eq!(output_file.task_id, "local_bash_spilled");
    let id = ToolUseId::new();
    let outcome = super::apply_tool_result_persistence_with_process_output(
        &orch,
        "Bash",
        &id,
        Some(THRESHOLD),
        "x".repeat(5_000),
        None,
        Some(&output_file),
    )
    .await;

    assert!(outcome.replaced);
    assert!(outcome.content.starts_with(PERSISTED_OUTPUT_OPEN));
    assert!(outcome.content.contains(&output_file.path));
    assert_eq!(std::fs::read_to_string(&output_path).unwrap().len(), 5_000);
    assert!(
        !tmp.path().join("projects").exists(),
        "the generic tool-use persistence path must not receive a duplicate"
    );
}

#[test]
fn process_output_file_from_data_accepts_legacy_and_2_1_263_names() {
    let legacy = json!({
        "outputTaskId": "legacy-id",
        "outputFilePath": "/tmp/legacy.out",
        "outputFileSize": 12,
    });
    let file = super::process_output_file_from_data(&legacy).expect("legacy");
    assert_eq!(file.task_id, "legacy-id");
    assert_eq!(file.path, "/tmp/legacy.out");
    assert_eq!(file.size, 12);

    let current = json!({
        "persistedOutputPath": "/tmp/current.out",
        "persistedOutputSize": 34,
    });
    let file = super::process_output_file_from_data(&current).expect("current");
    assert_eq!(file.task_id, "current");
    assert_eq!(file.path, "/tmp/current.out");
    assert_eq!(file.size, 34);
}

/// T6 — `U0u`: a block array containing an image (or document) is NEVER
/// persisted, however large its TEXT blocks are. The control below proves
/// the same array WITHOUT the media block does persist, so the assertion
/// isolates the guard rather than the size check.
#[tokio::test]
async fn media_bearing_block_arrays_are_never_persisted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let orch = orch_with(Arc::new(SizedTool), Some(tmp.path().to_path_buf()));
    let big_text = json!({ "type": "text", "text": "z".repeat(5_000) });
    let id = ToolUseId::new();

    for media in ["image", "document"] {
        let blocks = vec![big_text.clone(), json!({ "type": media })];
        let out = super::apply_tool_result_persistence(
            &orch,
            "Sized",
            &id,
            Some(THRESHOLD),
            "IGNORED".into(),
            Some(&blocks),
        )
        .await
        .content;
        assert_eq!(out, "IGNORED", "{media} block must suppress persistence");
    }
    assert!(!tmp.path().join("projects").exists());

    // Control: the identical array minus the media block IS persisted.
    let blocks = vec![big_text];
    let out = super::apply_tool_result_persistence(
        &orch,
        "Sized",
        &id,
        Some(THRESHOLD),
        "IGNORED".into(),
        Some(&blocks),
    )
    .await
    .content;
    assert!(out.starts_with(PERSISTED_OUTPUT_OPEN), "control: {out}");
    // An ARRAY body is written as pretty JSON under a `.json` stem (`kKr`).
    assert!(out.contains(&format!("{}.json", id.as_str())), "{out}");
}

/// An MCP-shaped result (array `data` ⇒ `content_blocks: Some(..)`) must
/// have its ARRAY dropped when the payload is persisted.
///
/// claude-code's `F0u` substitutes the ONE model-facing payload
/// (`{...e, content: a}`, where `content` is a string OR an array). LingXi
/// splits it across `content` and `content_blocks`, and the wire prefers
/// the array when present (`llm-runtime/src/convert.rs`:
/// `content_blocks.map_or_else(|| String(content), Array)`). Substituting
/// only `content` therefore wrote the file, fired the telemetry, and still
/// handed the model the full oversized array — the defect this pins.
#[tokio::test]
async fn persisting_an_mcp_array_result_drops_the_array() {
    struct McpArrayTool;
    #[async_trait]
    impl Tool for McpArrayTool {
        fn name(&self) -> &str {
            "mcp__srv__big"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object" }));
            &SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn is_mcp(&self) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn persistence_threshold(&self) -> Option<usize> {
            Some(THRESHOLD)
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
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
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "big".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let body = "y".repeat(THRESHOLD * 4);
            Ok(ToolCallResult {
                data: json!([{ "type": "text", "text": body }]),
                model_content: Some(body),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let orch = orch_with(Arc::new(McpArrayTool), Some(tmp.path().to_path_buf()));
    let uses = vec![(
        ToolUseId::new(),
        "mcp__srv__big".to_string(),
        json!({}),
        None,
    )];
    let (results, _, _, _) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");

    match &results[0] {
        ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } => {
            assert!(
                content.starts_with(PERSISTED_OUTPUT_OPEN),
                "oversized MCP result must be persisted, got: {content}"
            );
            assert!(
                content_blocks.is_none(),
                "the array must be dropped once the payload is substituted, \
                 else the wire sends it and the envelope is discarded"
            );
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

/// T7 — no `config_home` (library/test callers) is a STRICT no-op.
#[tokio::test]
async fn without_a_config_home_the_content_is_untouched() {
    let orch = orch_with(Arc::new(SizedTool), None);
    let content = dispatch_content(&orch, &use_of("Sized", 5_000)).await;
    assert_eq!(content, "x".repeat(5_000));
}

/// `Gzg` — a blank result becomes `(${toolName} completed with no output)`.
/// 1 933 occurrences in the real binary's own transcripts; the port emitted
/// 57 EMPTY Bash tool_results instead.
#[tokio::test]
async fn blank_results_become_the_no_output_sentinel() {
    let orch = orch_with(Arc::new(SizedTool), None);
    let content = dispatch_content(&orch, &use_of("Sized", 0)).await;
    assert_eq!(content, "(Sized completed with no output)");
}
