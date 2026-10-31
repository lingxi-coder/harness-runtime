//! Strict opt-in checkpoints for a host-owned, persistent Local App Chat.

use super::super::*;
use lingxi_core::types::{ContentBlock, ImageSource};
use session::jsonl::CapturedChatTranscript;
use std::collections::{HashMap, HashSet};

const UNAVAILABLE: &str = "chat_transcript_unavailable";
const EXTERNAL_RESULT: &str = "chat_transcript_external_result_unmanaged";

fn validate_inline_history(history: &[ConversationMessage]) -> Result<(), String> {
    let mut message_ids = HashSet::new();
    let mut calls = HashMap::<String, Option<String>>::new();
    let mut resolved = HashSet::new();
    for message in history {
        if !message_ids.insert(message.id()) || message.is_compact_summary() {
            return Err(UNAVAILABLE.into());
        }
        let (assistant, content) = match message {
            ConversationMessage::User { content, .. } => (false, content),
            ConversationMessage::Assistant { content, .. } => (true, content),
            // API/system and compaction messages have additional restoration
            // state outside this deliberately narrow Chat checkpoint contract.
            ConversationMessage::System { .. } => return Err(UNAVAILABLE.into()),
        };
        for block in content {
            match block {
                ContentBlock::ToolUse {
                    id,
                    name: _,
                    provider_id,
                    ..
                } => {
                    if !assistant {
                        return Err(EXTERNAL_RESULT.into());
                    }
                    if calls.insert(id.to_string(), provider_id.clone()).is_some() {
                        return Err(UNAVAILABLE.into());
                    }
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    provider_tool_use_id,
                    content,
                    content_blocks,
                    ..
                } => {
                    if assistant
                        || calls.get(&tool_use_id.to_string()) != Some(provider_tool_use_id)
                        || !resolved.insert(tool_use_id.to_string())
                    {
                        return Err(UNAVAILABLE.into());
                    }
                    tool_api::persistent_chat::validate_inline_result(
                        content,
                        content_blocks.as_ref(),
                    )
                    .map_err(str::to_owned)?;
                }
                ContentBlock::Image {
                    source: ImageSource::Url { .. },
                }
                | ContentBlock::ProviderContent { .. }
                | ContentBlock::ServerToolUse { .. }
                | ContentBlock::AdvisorToolResult { .. }
                | ContentBlock::MediaAnalysis { .. } => return Err(EXTERNAL_RESULT.into()),
                ContentBlock::Text { .. }
                | ContentBlock::TextJsUtf16 { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ConnectorText { .. } => {}
                ContentBlock::Image {
                    source: ImageSource::Base64 { .. },
                }
                | ContentBlock::Document { .. } => {
                    let serialized = serde_json::to_string(block).map_err(|_| EXTERNAL_RESULT)?;
                    let value =
                        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&serialized)
                            .map_err(|_| EXTERNAL_RESULT)?;
                    let blocks =
                        lingxi_core::types::utf16_json::Utf16JsonProjection::array(vec![value])
                            .map_err(|_| EXTERNAL_RESULT)?;
                    tool_api::persistent_chat::validate_inline_blocks(&blocks)
                        .map_err(str::to_owned)?;
                }
            }
        }
    }
    if calls.len() != resolved.len() {
        return Err(UNAVAILABLE.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
        MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
    };
    use async_trait::async_trait;
    use lingxi_core::host::OrchestratorHandle;
    use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, ToolUseId};
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
        ValidationError,
    };

    struct InlineRoundTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for InlineRoundTool {
        fn name(&self) -> &str {
            "InlineRound"
        }
        fn supports_persistent_chat(&self) -> bool {
            true
        }
        fn input_schema(&self) -> &Source {
            static SCHEMA: std::sync::OnceLock<Source> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| Source::parse(r#"{"type":"object"}"#).unwrap())
        }
        fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
            tool_api::tool_trait::ToolInputRepairMetadata::Known {
                schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
                schema_provenance:
                    tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            500_000
        }
        fn persistence_threshold(&self) -> Option<usize> {
            Some(32)
        }
        fn is_concurrency_safe(&self, _: &Source) -> bool {
            true
        }
        fn is_read_only(&self, _: &Source) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &Source,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Source,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "fixture".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(&self, _: &Source, _: &DescriptionOptions) -> String {
            "inline result".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Source {
            "Return inline text.".into()
        }
        async fn call(
            &self,
            _: Source,
            ctx: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            assert_eq!(
                ctx.tool_execution_policy,
                lingxi_core::host::tool_invoker::ToolExecutionPolicy::LocalAppChat
            );
            self.0.fetch_add(1, Ordering::SeqCst);
            let text: Source = "retained-inline-".repeat(10_000).into();
            Ok(ToolCallResult {
                data: text.clone(),
                model_content: Some(text),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
                subagent_hooks_fired: false,
            })
        }
    }

    async fn fixture() -> (
        tempfile::TempDir,
        Arc<ConversationOrchestrator>,
        Arc<JsonlWriter>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let directory = root
            .path()
            .join("apps/test-app/agent/agui-v1/chats")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.jsonl", session_id.as_uuid()));
        let writer = Arc::new(
            JsonlWriter::open_persistent_chat(
                path,
                Arc::new(platform_posix::fs::PosixFileSystem::new(
                    root.path().to_path_buf(),
                )),
                None,
            )
            .await
            .unwrap(),
        );
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            api,
            Arc::new(MockStreamingApiClient::empty()),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_session_id(session_id)
        .with_persistent_app_chat()
        .with_jsonl_writer(writer.clone());
        (root, Arc::new(orch), writer)
    }

    fn assistant(content: Vec<ContentBlock>) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content,
            // External assistant messages use the synthetic transcript form.
            stop_reason: Some("stop_sequence".into()),
            resumable: None,
            per_turn_effort: None,
        }
    }

    #[tokio::test]
    async fn verified_capture_replays_the_same_message_id_and_content_and_seals() {
        let (_root, orch, writer) = fixture().await;
        assert!(!orch.side_chat);
        assert!(orch.is_persistent_app_chat());
        let user = ConversationMessage::user(MessageId::new(), "question".into());
        orch.append_external_history_message(user.clone()).await;
        let initialized = orch.verify_persistent_chat_checkpoint(false).await.unwrap();
        let replayed =
            ConversationOrchestrator::replay_persistent_chat_snapshot(&initialized).unwrap();
        assert_eq!(replayed.state.history, vec![user]);

        let answer = assistant(vec![ContentBlock::Text {
            text: "answer".into(),
            citations: None,
        }]);
        orch.append_external_history_message(answer.clone()).await;
        let finished = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        assert!(finished.source().starts_with(initialized.source()));
        let replayed =
            ConversationOrchestrator::replay_persistent_chat_snapshot(&finished).unwrap();
        assert_eq!(replayed.state.history.last(), Some(&answer));
        assert!(writer
            .append_custom_title(
                &finished.session_id().to_string(),
                Source::from("late metadata")
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn inline_mcp_media_cold_checkpoint_replays_without_external_artifacts() {
        let (_root, orch, _writer) = fixture().await;
        let call_id = ToolUseId::new();
        let user = ConversationMessage::user(MessageId::new(), "read media".into());
        let call = assistant(vec![ContentBlock::ToolUse {
            id: call_id.clone(),
            name: "mcp__memory__read".into(),
            input: Source::parse("{}").unwrap(),
            provider_id: Some("provider-media".into()),
        }]);
        let mut result = ConversationMessage::user(MessageId::new(), String::new());
        if let ConversationMessage::User { content, .. } = &mut result {
            *content = vec![ContentBlock::ToolResult {
                tool_use_id: call_id,
                provider_tool_use_id: Some("provider-media".into()),
                content: "inline media".into(), is_error: Some(false),
                content_blocks: Some(Source::parse(r#"[{"type":"text","text":"caption"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}},{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERg=="}}]"#).unwrap()),
            }];
        }
        for message in [user.clone(), call.clone(), result.clone()] {
            orch.append_external_history_message(message).await;
        }
        let captured = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        let cold = ConversationOrchestrator::replay_persistent_chat_snapshot(&captured).unwrap();
        assert_eq!(cold.state.history, vec![user, call, result]);
    }

    #[tokio::test]
    async fn actual_inline_tool_round_cold_resume_and_next_turn_preserve_exact_history() {
        let (root, orch, writer) = fixture().await;
        let mut orch = Arc::try_unwrap(orch).unwrap_or_else(|_| panic!("fixture has one owner"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(InlineRoundTool(calls.clone())));
        let registry = Arc::new(registry);
        orch.tools = registry.clone();
        orch.config.system_prompt_override = Some("Test Chat.".into());
        orch.config.append_system_prompt =
            Some("Selected App skill: use the authorized InlineRound tool.".into());
        let runtime_home = root.path().join("runtime");
        orch.config_home = Some(runtime_home.clone());
        let response = |text: &str| {
            mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: text.into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            )
        };
        orch.api = Arc::new(MockApiClient::new(vec![
            mock_message_response(
                vec![llm_runtime::ContentBlock::ToolCall {
                    id: "provider-inline-round".into(),
                    name: "InlineRound".into(),
                    input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
                }],
                Some("tool_use"),
            ),
            response("tool complete"),
        ]));
        let orch = ConversationOrchestrator::into_shared(orch);
        orch.run_turn("Run the inline tool.").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let original = orch.snapshot_history().await;
        assert!(original.iter().any(|message| matches!(message, ConversationMessage::Assistant {
            content, stop_reason: Some(reason), ..
        } if reason == "tool_use" && content.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. })))));
        let captured = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        assert!(captured
            .source()
            .contains("retained-inline-retained-inline-"));
        assert!(!captured.source().contains("<persisted-output>"));
        let replayed =
            ConversationOrchestrator::replay_persistent_chat_snapshot(&captured).unwrap();
        assert_eq!(replayed.state.history, original);
        let runtime = replayed.handle_runtime_snapshot();
        let config = orch.config.clone();
        drop(orch);
        drop(writer);
        let reopened = Arc::new(
            JsonlWriter::open_persistent_chat(
                captured.path().to_path_buf(),
                Arc::new(platform_posix::fs::PosixFileSystem::new(
                    root.path().to_path_buf(),
                )),
                Some(captured.checkpoint().clone()),
            )
            .await
            .unwrap(),
        );
        let restored = ConversationOrchestrator::new_with_streaming(
            config,
            Arc::new(MockApiClient::new(vec![response(
                "continued after restore",
            )])),
            Arc::new(MockStreamingApiClient::empty()),
            registry,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_session_id(SessionId::from_uuid(captured.session_id()))
        .with_jsonl_writer(reopened)
        .with_persistent_app_chat();
        let restored = ConversationOrchestrator::into_shared(restored);
        restored
            .resume_session(
                SessionId::from_uuid(captured.session_id()),
                replayed.state.history,
                replayed.last_message_uuid.map(|id| id.to_string()),
                None,
                runtime,
            )
            .await
            .unwrap();
        restored
            .run_turn("Continue the same conversation.")
            .await
            .unwrap();
        let final_capture = restored
            .verify_persistent_chat_checkpoint(true)
            .await
            .unwrap();
        let final_state =
            ConversationOrchestrator::replay_persistent_chat_snapshot(&final_capture).unwrap();
        assert!(final_state.state.history.starts_with(&original));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "recovery must not execute a historical tool"
        );
        assert!(
            !runtime_home.join("projects").exists(),
            "inline output must not create a generic spill directory"
        );
    }

    #[tokio::test]
    async fn structured_output_terminal_tool_result_is_closed_before_checkpoint() {
        use lingxi_core::host::json_schema_validator::{
            CompiledJsonSchema, JsonSchemaValidation, SchemaWorkerError,
        };
        struct ValidPlan;
        #[async_trait]
        impl CompiledJsonSchema for ValidPlan {
            async fn validate(
                &self,
                _: &Source,
            ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
                Ok(JsonSchemaValidation::Valid)
            }
        }
        let (_root, orch, _writer) = fixture().await;
        let mut orch = Arc::try_unwrap(orch).unwrap_or_else(|_| panic!("fixture has one owner"));
        let slot = Arc::new(std::sync::Mutex::new(None));
        let schema = Source::parse(
            r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}"#,
        )
        .unwrap();
        let tool = crate::structured_output::StructuredOutputTool::new(
            schema,
            Arc::new(ValidPlan),
            slot.clone(),
        );
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(tool));
        orch.tools = Arc::new(registry);
        orch.config.structured_output_enabled = true;
        orch.config.system_prompt_override = Some("Test structured Chat.".into());
        orch.config.append_system_prompt =
            Some("Selected App skill: return the authorized structured answer.".into());
        orch.api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::ToolCall {
                id: "provider-structured-result".into(),
                name: "StructuredOutput".into(),
                input: lingxi_llm_client::protocol::JsonPayload::parse(r#"{"answer":"done"}"#)
                    .unwrap(),
            }],
            Some("tool_use"),
        )]));
        let orch = ConversationOrchestrator::into_shared(orch);
        orch.run_turn("Return the final structured answer.")
            .await
            .unwrap();
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().string_at("/answer"),
            Some("done")
        );
        let live = orch.snapshot_history().await;
        assert!(live.iter().any(
            |message| matches!(message, ConversationMessage::User { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))
        ));
        let captured = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        let cold = ConversationOrchestrator::replay_persistent_chat_snapshot(&captured).unwrap();
        assert_eq!(cold.state.history, live);
    }

    #[tokio::test]
    async fn an_in_memory_assistant_without_its_durable_row_cannot_checkpoint() {
        let (_root, orch, writer) = fixture().await;
        orch.append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            "question".into(),
        ))
        .await;
        let accepted_user = orch.verify_persistent_chat_checkpoint(false).await.unwrap();
        // This is the actual state left by a swallowed assistant append error:
        // the live row survived but the physical transcript contains the prompt.
        orch.session
            .lock()
            .await
            .history
            .push(assistant(vec![ContentBlock::Text {
                text: "not on disk".into(),
                citations: None,
            }]));
        assert_eq!(
            orch.verify_persistent_chat_checkpoint(true)
                .await
                .unwrap_err(),
            UNAVAILABLE
        );
        assert_eq!(
            std::fs::read(accepted_user.path()).unwrap(),
            accepted_user.source().as_bytes()
        );
        assert!(writer
            .append_custom_title(
                &accepted_user.session_id().to_string(),
                Source::from("late")
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn strict_replay_rejects_an_unknown_or_malformed_record() {
        for suffix in ["{\"type\":\"future-record\"}\n", "{broken\n"] {
            let (_root, orch, writer) = fixture().await;
            orch.append_external_history_message(ConversationMessage::user(
                MessageId::new(),
                "question".into(),
            ))
            .await;
            let initialized = orch.verify_persistent_chat_checkpoint(false).await.unwrap();
            // Fault injection bypasses the cooperating writer's lifetime lock.
            std::fs::OpenOptions::new()
                .append(true)
                .open(initialized.path())
                .unwrap()
                .write_all(suffix.as_bytes())
                .unwrap();
            let captured = writer.capture_persistent_chat_snapshot(true).await.unwrap();
            assert!(ConversationOrchestrator::replay_persistent_chat_snapshot(&captured).is_err());
        }
    }

    #[tokio::test]
    async fn checkpoint_waits_for_the_turn_boundary() {
        let (_root, orch, _writer) = fixture().await;
        orch.append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            "question".into(),
        ))
        .await;
        let turn = orch.turn_gate.lock().await;
        let worker = orch.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let checkpoint = tokio::spawn(async move {
            let _ = entered_tx.send(());
            worker.verify_persistent_chat_checkpoint(true).await
        });
        entered_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert!(!checkpoint.is_finished());
        drop(turn);
        assert!(checkpoint.await.unwrap().is_ok());
    }

    #[test]
    fn inline_mcp_is_name_independent_and_tool_pairs_are_strict() {
        let id = ToolUseId::new();
        let invocation = |name: &str| {
            assistant(vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: name.into(),
                input: Source::parse("{}").unwrap(),
                provider_id: Some("provider-call".into()),
            }])
        };
        assert_eq!(
            validate_inline_history(&[invocation("mcp__server__read")]).unwrap_err(),
            UNAVAILABLE
        );
        assert_eq!(
            validate_inline_history(&[invocation("Shell")]).unwrap_err(),
            UNAVAILABLE
        );
        let mut result = ConversationMessage::user(MessageId::new(), String::new());
        if let ConversationMessage::User { content, .. } = &mut result {
            *content = vec![ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: Source::from("inline result"),
                is_error: Some(false),
                provider_tool_use_id: Some("provider-call".into()),
                content_blocks: None,
            }];
        }
        assert!(validate_inline_history(&[invocation("Shell"), result.clone()]).is_ok());
        let mut image_result = result.clone();
        if let ConversationMessage::User { content, .. } = &mut image_result {
            if let ContentBlock::ToolResult { content_blocks, .. } = &mut content[0] {
                *content_blocks = Some(Source::parse(r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}]"#).unwrap());
            }
        }
        assert!(
            validate_inline_history(&[invocation("mcp__server__read"), image_result.clone()])
                .is_ok()
        );
        if let ConversationMessage::User { content, .. } = &mut image_result {
            if let ContentBlock::ToolResult { content_blocks, .. } = &mut content[0] {
                *content_blocks = Some(Source::parse(r#"[{"type":"image","source":{"type":"url","url":"https://example.test/image.png"}}]"#).unwrap());
            }
        }
        assert!(validate_inline_history(&[invocation("mcp__server__read"), image_result]).is_err());
        let mut wrong_provider = result.clone();
        if let ConversationMessage::User { content, .. } = &mut wrong_provider {
            if let ContentBlock::ToolResult {
                provider_tool_use_id,
                ..
            } = &mut content[0]
            {
                *provider_tool_use_id = Some("another-provider-call".into());
            }
        }
        assert!(validate_inline_history(&[invocation("Shell"), wrong_provider]).is_err());
        assert!(validate_inline_history(&[invocation("Shell"), result.clone(), result]).is_err());
    }
}

impl ConversationOrchestrator {
    /// Select the current host-owned Local App Chat contract. The tool catalog
    /// and its construction contexts must use the same explicit profile.
    #[must_use]
    pub fn with_persistent_app_chat(mut self) -> Self {
        self.persistent_app_chat = true;
        self.idle_compaction_enabled = false;
        self.compaction_runtime.compaction = None;
        self
    }

    #[must_use]
    pub fn is_persistent_app_chat(&self) -> bool {
        self.persistent_app_chat
    }

    pub(crate) fn validate_persistent_chat_history(
        history: &[ConversationMessage],
    ) -> Result<(), String> {
        validate_inline_history(history)
    }

    /// Replay only the exact captured bytes. This function performs no I/O and
    /// never dispatches deferred tools. Unknown rows, malformed tails, forks,
    /// compaction and untracked external results fail closed.
    pub fn replay_persistent_chat_snapshot(
        captured: &CapturedChatTranscript,
    ) -> Result<crate::resume::ReplayedSession, String> {
        let source = captured.source();
        if source.is_empty() || !source.ends_with('\n') {
            return Err(UNAVAILABLE.into());
        }
        let session_id = captured.session_id().to_string();
        let loaded = session::jsonl::route_lines(source);
        if loaded.malformed_line_count != 0 || loaded.messages_in_order.is_empty() {
            return Err(UNAVAILABLE.into());
        }
        // The tolerant ordinary loader ignores unknown records. A checkpoint
        // must not turn an unrecognized suffix into trusted resume metadata.
        for line in source.lines().filter(|line| !line.trim().is_empty()) {
            let row = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line)
                .map_err(|_| UNAVAILABLE)?;
            if row.string_at("/sessionId") != Some(session_id.as_str())
                || !matches!(
                    row.string_at("/type"),
                    Some(
                        "user"
                            | "assistant"
                            | "attachment"
                            | "custom-title"
                            | "ai-title"
                            | "last-prompt"
                    )
                )
            {
                return Err(UNAVAILABLE.into());
            }
        }
        let mut outer_rows = HashMap::<String, String>::new();
        for row in &loaded.messages_in_order {
            if row.session_id != session_id
                || row.is_sidechain
                || !uuid::Uuid::parse_str(&row.uuid).is_ok_and(|id| id.to_string() == row.uuid)
                || outer_rows.contains_key(&row.uuid)
                || (row.parent_uuid.is_none() && !outer_rows.is_empty())
                || row
                    .parent_uuid
                    .as_ref()
                    .is_some_and(|parent| !outer_rows.contains_key(parent))
            {
                return Err(UNAVAILABLE.into());
            }
            if matches!(row.message_type.as_str(), "user" | "assistant")
                && row.message.string_at("/role") != Some(row.message_type.as_str())
            {
                return Err(UNAVAILABLE.into());
            }
            if row.message_type == "assistant"
                && !row.message.string_at("/id").is_some_and(|id| {
                    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
                })
            {
                return Err(UNAVAILABLE.into());
            }
            if let Some(parent) = row.extra.string_at("/sourceToolAssistantUUID") {
                if outer_rows.get(parent).map(String::as_str) != Some("assistant") {
                    return Err(UNAVAILABLE.into());
                }
            }
            outer_rows.insert(row.uuid.clone(), row.message_type.clone());
        }
        let replayed = crate::resume::replay_persistent_app_chat_session(
            captured.path().to_path_buf(),
            source.to_owned(),
            loaded,
            captured.session_id(),
        )
        .map_err(|_| UNAVAILABLE)?;
        if replayed.state.history != replayed.display_history
            || !replayed.runtime_metadata.deferred_tools.is_empty()
            || replayed.runtime_metadata.compaction_tracking.compacted
            || replayed.state.active_goal.is_some()
            || replayed.state.compacted_user_turns != 0
            || replayed.state.thinking_signature_stripped
            || !replayed.state.thinking_stripped_messages.is_empty()
        {
            return Err(UNAVAILABLE.into());
        }
        validate_inline_history(&replayed.state.history)?;
        super::super::persistent_chat_context::replay_context_receipts(captured)?;
        Ok(replayed)
    }

    /// Checkpoint an idle, independently owned side Chat. The turn gate, the
    /// transcript-append gate and the live session lock are held through the
    /// writer's atomic capture and exact engine-history comparison. A terminal
    /// (`seal = true`) call closes the writer before releasing those guards.
    ///
    /// The embedding host must persist `captured.checkpoint()` in its authority
    /// before acknowledging initialization or publishing a successful result.
    /// Recovery opens a controlled writer with that proof, captures it, and
    /// resumes from `replay_persistent_chat_snapshot` using the SAME writer Arc.
    pub async fn verify_persistent_chat_checkpoint(
        &self,
        seal: bool,
    ) -> Result<CapturedChatTranscript, String> {
        if !self.is_persistent_app_chat() {
            return Err(UNAVAILABLE.into());
        }
        let writer = self.transcript.jsonl_writer.as_ref().ok_or(UNAVAILABLE)?;
        if !writer.is_persistent_chat() {
            return Err(UNAVAILABLE.into());
        }
        self.cancel_idle_compaction();
        let _turn = self.turn_gate.lock().await;
        let _append = self.transcript.storage_append_lock.lock().await;
        let live = self.session.lock().await;
        let result = async {
            if live.active_goal.is_some()
                || live.compacted_user_turns != 0
                || live.thinking_signature_stripped
                || !live.thinking_stripped_messages.is_empty()
                || self
                    .transcript
                    .storage_pauses
                    .lock()
                    .await
                    .contains_key(&live.session_id.as_uuid().to_string())
            {
                return Err(UNAVAILABLE.to_owned());
            }
            validate_inline_history(&live.history)?;
            // Admission is a capability of the implementation, never a tool
            // name allowlist. Unknown/dynamic tools fail before any effects.
            if self
                .tools
                .available_tools(&tool_api::tool_trait::ToolStaticContext::default())
                .iter()
                .any(|tool| !tool.supports_persistent_chat())
            {
                return Err(EXTERNAL_RESULT.to_owned());
            }
            let captured = writer
                .capture_persistent_chat_snapshot(seal)
                .await
                .map_err(|_| UNAVAILABLE)?;
            if captured.session_id() != live.session_id.as_uuid() {
                return Err(UNAVAILABLE.to_owned());
            }
            let replayed = Self::replay_persistent_chat_snapshot(&captured)?;
            self.verify_persistent_chat_context(&captured)?;
            if live.history != replayed.state.history
                || live.model_context_excluded_messages
                    != replayed.state.model_context_excluded_messages
                || live.transcript_only_messages != replayed.state.transcript_only_messages
                || live.compact_summary_messages != replayed.state.compact_summary_messages
                || live.virtual_user_messages != replayed.state.virtual_user_messages
                || live.hook_message_grouping != replayed.state.hook_message_grouping
                || live.native_request_history != replayed.state.native_request_history
            {
                return Err(UNAVAILABLE.to_owned());
            }
            Ok(captured)
        }
        .await;
        if result.is_err() {
            let _ = writer.seal_persistent_chat().await;
        }
        result
    }
}
