//! Real sandbox producer -> JSONL -> public loader/with_resume -> physical request.
use crate::conversation::NativeSandboxHostFacts;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig};
use async_trait::async_trait;
use lingxi_core::host::FileSystem;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection as Source, ConversationMessage, SessionId,
};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::sync::Arc;
use tool_api::tool_trait::{
    generated_builtin_schema, DescriptionOptions, PromptOptions, Tool, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::{ToolCallResult, ToolError};

const MODEL: &str = "claude-opus-4-7";
struct BashFixture(Source);
#[async_trait]
impl Tool for BashFixture {
    fn name(&self) -> &str {
        "Bash"
    }
    fn input_schema(&self) -> &Source {
        &self.0
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
    fn is_concurrency_safe(&self, _: &Source) -> bool {
        false
    }
    fn is_read_only(&self, _: &Source) -> bool {
        false
    }
    async fn check_permissions(
        &self,
        _: &Source,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        unreachable!("the fixture never invokes Bash")
    }
    async fn description(&self, _: &Source, _: &DescriptionOptions) -> String {
        "fixture shell catalog only".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Source {
        "fixture shell catalog only".into()
    }
    async fn call(
        &self,
        _: Source,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        unreachable!("the fixture never invokes Bash")
    }
}
fn tools() -> Arc<tool_api::registry::ToolRegistry> {
    let mut tools = tool_api::registry::ToolRegistry::new();
    tools.register_builtin(Arc::new(BashFixture(generated_builtin_schema(
        &json!({"type":"object"}),
    ))));
    Arc::new(tools)
}
fn facts() -> NativeSandboxHostFacts {
    NativeSandboxHostFacts {
        sandbox_available: true,
        filesystem_read: Source::parse(r#"{"denyOnly":[],"allowWithinDeny":[]}"#).unwrap(),
        filesystem_write: Source::parse(r#"{"allowOnly":["/workspace"],"denyWithinAllow":[]}"#)
            .unwrap(),
        network_restriction_config: None,
        allow_unix_sockets: None,
        ignore_violations: None,
        sandbox_settings: None,
        allow_unsandboxed_commands: false,
        allow_apple_events: false,
        allow_local_binding: false,
        platform: Some("linux".into()),
        auto_mode: false,
        filesystem_policy_relaxed: false,
        temp_paths: vec![],
        boundary_note_enabled: false,
        command_network_lists_offered: false,
        sandbox_command_changes_settings: true,
        interactive_session: false,
        session_kind_present: false,
        settings_layers: vec![],
    }
}
fn config() -> OrchestratorConfig {
    OrchestratorConfig {
        model: MODEL.into(),
        system_prompt_override: Some("cold sandbox replay".into()),
        ..Default::default()
    }
}
fn response() -> llm_runtime::HistoryResponse {
    let mut response = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    response.model = MODEL.into();
    response
}
async fn run(orch: &ConversationOrchestrator, streaming: bool, prompt: &str) {
    if streaming {
        orch.run_turn_streaming(prompt).await.unwrap();
    } else {
        orch.run_turn(prompt).await.unwrap();
    }
}
async fn one_case(streaming: bool) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(root));
    let sid = SessionId::new();
    let cwd_str = cwd.to_string_lossy().into_owned();
    let transcript =
        session::jsonl::path::session_path(&home, &cwd_str, &sid.as_uuid().to_string());
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let writer = Arc::new(JsonlWriter::new(transcript.clone(), fs.clone()));
    let api = Arc::new(MockApiClient::new(vec![response(), response()]));
    let stream = Arc::new(MockStreamingApiClient::with_turns(
        (0..3)
            .map(|i| {
                vec![
                    message_start(&format!("sandbox-warm-{i}"), MODEL),
                    content_block_start_text(0),
                    text_delta(0, "done"),
                    content_block_stop(0),
                    message_delta_stop("end_turn"),
                    message_stop(),
                ]
            })
            .collect(),
    ));
    let warm = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            config(),
            api.clone(),
            stream.clone(),
            tools(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_session_id(sid)
        .with_native_sandbox_host_facts_provider(facts)
        .with_jsonl_writer(writer.clone()),
    );
    warm.initialize_conversation_mode().await.unwrap();
    run(&warm, streaming, "initial sandbox turn").await;
    run(&warm, streaming, "unchanged warm followup").await;
    let rows = JsonlReader::new(transcript.clone(), fs.clone())
        .read_all()
        .await
        .unwrap();
    let policy: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.message_type == "attachment"
                && r.attachment_projection
                    .as_ref()
                    .and_then(|p| p.string_at("/type"))
                    == Some("sandbox_instructions")
        })
        .collect();
    assert_eq!(
        policy.len(),
        1,
        "unchanged warm policy is announced exactly once"
    );
    let id = policy[0].uuid.clone();
    let original_id = lingxi_core::types::MessageId::from_uuid(uuid::Uuid::parse_str(&id).unwrap());
    let raw = warm
        .snapshot_history()
        .await
        .into_iter()
        .find(|m| m.id() == original_id)
        .unwrap()
        .text_content();
    let warm_calls: Vec<Vec<ConversationMessage>> = if streaming {
        stream
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect()
    } else {
        api.captured_msgs().await
    };
    let warm_policy_index = warm_calls[1].iter().position(|m| m.id() == original_id);
    let replayed = crate::resume::replay_session_state(&home, &cwd_str, sid.as_uuid(), fs.clone())
        .await
        .unwrap();
    let restored_by_loader = replayed.state.history.iter().any(|m| m.id() == original_id);
    let cold_api = Arc::new(MockApiClient::new(vec![response()]));
    let mut cold = ConversationOrchestrator::with_resume(
        config(),
        sid.as_uuid(),
        home,
        cwd_str,
        fs.clone(),
        cold_api.clone(),
        tools(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
        Some(writer),
    )
    .await
    .unwrap()
    .with_native_sandbox_host_facts_provider(facts);
    cold.streaming_api = stream.clone();
    let cold = ConversationOrchestrator::into_shared(cold);
    let restored_by_constructor = cold
        .snapshot_history()
        .await
        .iter()
        .any(|m| m.id() == original_id);
    run(&cold, streaming, "cold followup with unchanged sandbox").await;
    let cold_calls = if streaming {
        stream
            .captured_calls()
            .await
            .into_iter()
            .skip(2)
            .map(|call| call.messages)
            .collect::<Vec<_>>()
    } else {
        cold_api.captured_msgs().await
    };
    assert_eq!(cold_calls.len(), 1);
    let cold_policy_ids: Vec<_> = cold_calls[0]
        .iter()
        .filter(|m| m.text_content() == raw)
        .map(|m| m.id().to_string())
        .collect();
    let cold_policy_index = cold_calls[0].iter().position(|m| m.text_content() == raw);
    let rows_after = JsonlReader::new(transcript, fs).read_all().await.unwrap();
    let sandbox_uuids: Vec<_> = rows_after
        .iter()
        .filter(|r| {
            r.message_type == "attachment"
                && r.attachment_projection
                    .as_ref()
                    .and_then(|p| p.string_at("/type"))
                    == Some("sandbox_instructions")
        })
        .map(|r| r.uuid.clone())
        .collect();
    assert!(restored_by_loader && restored_by_constructor);
    assert_eq!(
        cold_policy_index, warm_policy_index,
        "cold policy keeps its historical physical position"
    );
    assert_eq!(
        sandbox_uuids,
        vec![id.clone()],
        "unchanged policy is not announced again"
    );
    assert_eq!(cold_policy_ids, vec![original_id.to_string()]);
    json!({"warm_driver":if streaming {"streaming"} else {"batched"},"original_policy_uuid":id,
        "unchanged_warm_policy_rows":policy.len(),"original_restored_by_public_loader":restored_by_loader,
        "original_restored_by_with_resume":restored_by_constructor,"cold_physical_policy_uuids":cold_policy_ids,
        "warm_second_request_policy_index":warm_policy_index,"cold_request_policy_index":cold_policy_index,
        "all_durable_policy_uuids_after_cold":sandbox_uuids,
        "passed":restored_by_loader&&restored_by_constructor&&cold_calls[0].iter().any(|m|m.id()==original_id)})
}
#[test]
fn real_sandbox_policy_keeps_uuid_order_and_dedup_across_cold_both_drivers() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    for streaming in [false, true] {
                        let result = one_case(streaming).await;
                        assert_eq!(result["passed"], true, "{result:#}");
                    }
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn visible_policy_render_caches_restore_exact_utf16_and_reject_unknown_or_empty_rows() {
    let session_id = uuid::Uuid::new_v4();
    for kind in ["sandbox_instructions", "attention_budget", "proactivity"] {
        let id = uuid::Uuid::new_v4();
        let source = format!(
            r#"{{"type":"attachment","uuid":"{id}","parentUuid":null,"isSidechain":false,"timestamp":"2026-11-13T00:00:00Z","cwd":"/workspace","version":"test","sessionId":"{session_id}","message":null,"attachment":{{"type":"{kind}","content":"raw-{kind}\ud800"}},"rendered":[{{"content":"view-{kind}\ud800"}}],"renderedRole":"system"}}"#
        );
        let row: session::JsonlMessage = serde_json::from_str(&source).unwrap();
        let restored = super::state_from_messages(session_id, std::slice::from_ref(&row));
        assert_eq!(restored.history.len(), 1, "{kind}");
        assert_eq!(restored.history[0].id().as_uuid(), id);
        let ConversationMessage::User {
            content,
            is_meta: true,
            ..
        } = &restored.history[0]
        else {
            panic!("policy cache must restore its meta-user identity");
        };
        let [lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        }] = content.as_slice()
        else {
            panic!("policy cache must retain exact UTF-16");
        };
        assert_eq!(utf16_code_units.last(), Some(&0xd800));
        assert_eq!(
            super::model_reminder_attachments_from_messages(std::slice::from_ref(&row))
                .get(&restored.history[0].id()),
            row.attachment_projection.as_ref(),
        );
        assert_eq!(
            super::mod_attachment_descriptors_from_messages(std::slice::from_ref(&row))
                .get(&restored.history[0].id()),
            Some(&(kind.to_owned(), json!({"kind":"engine"}))),
        );
        for cache in ["[]", r#"[{"content":""}]"#, r#"[{"content":7}]"#] {
            let mut invalid = row.clone();
            invalid
                .extra
                .set_field("rendered", Source::parse(cache).unwrap())
                .unwrap();
            assert!(super::state_from_messages(session_id, &[invalid])
                .history
                .is_empty());
        }
        let mut unknown = row;
        unknown
            .attachment_projection
            .as_mut()
            .unwrap()
            .set_field("type", Source::from("unknown_policy_kind"))
            .unwrap();
        assert!(super::state_from_messages(session_id, &[unknown])
            .history
            .is_empty());
    }
}
