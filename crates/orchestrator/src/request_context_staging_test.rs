use super::*;
use crate::conversation::ModelCallPath;
use crate::prompt::skill_listing::{SkillListingEntry, SkillListingProvider, SkillListingSnapshot};
use crate::prompt::MemoryFile;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use async_trait::async_trait;
use lingxi_core::host::instructions::InstructionRendering;
use lingxi_core::host::{FileSystem, PermissionGate};
use memory::lingxi_md::LingxiMdTier;
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::path::Path;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

fn orchestrator(
    cwd: &Path,
    transcript: &Path,
    filesystem: Arc<dyn FileSystem>,
) -> ConversationOrchestrator {
    orchestrator_with_api(
        cwd,
        transcript,
        filesystem,
        Arc::new(MockApiClient::new(Vec::new())),
    )
}

fn orchestrator_with_api(
    cwd: &Path,
    transcript: &Path,
    filesystem: Arc<dyn FileSystem>,
    api: Arc<MockApiClient>,
) -> ConversationOrchestrator {
    orchestrator_with_api_and_tools(
        cwd,
        transcript,
        filesystem,
        api,
        Arc::new(ToolRegistry::new()),
    )
}

fn orchestrator_with_api_and_tools(
    cwd: &Path,
    transcript: &Path,
    filesystem: Arc<dyn FileSystem>,
    api: Arc<MockApiClient>,
    tools: Arc<ToolRegistry>,
) -> ConversationOrchestrator {
    let memory = StaticMemoryProvider::with_files(vec![MemoryFile {
        source_content: None,
        parent: None,
        path: cwd.join("AGENTS.md"),
        body: "staged instructions".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: None,
        raw_content: "staged instructions".into(),
        content_differs_from_disk: false,
    }]);
    ConversationOrchestrator::new(
        crate::OrchestratorConfig {
            context_rendering: InstructionRendering::Announced,
            ..Default::default()
        },
        api,
        tools,
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(memory),
        cwd.to_owned(),
    )
    .with_jsonl_writer(Arc::new(JsonlWriter::new(
        transcript.to_owned(),
        filesystem,
    )))
}

fn native_policy_orchestrator(
    cwd: &Path,
    transcript: &Path,
    filesystem: Arc<dyn FileSystem>,
) -> (
    ConversationOrchestrator,
    Arc<permission::PolicyPermissionGate>,
) {
    let permission_gate = Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(
            permission::PermissionPolicy::from_rules(
                permission::PermissionMode::Default,
                Vec::new(),
            )
            .with_bypass_available(true),
        ),
        Arc::new(NoOpPermissionGate),
    ));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(SkillListingGateTool { name: "Bash" }));
    tools.register_builtin(Arc::new(SkillListingGateTool { name: "Edit" }));
    let mut orchestrator = orchestrator_with_api_and_tools(
        cwd,
        transcript,
        filesystem,
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tools),
    );
    orchestrator.perms = permission_gate.clone();
    orchestrator.native_policy_host_facts_provider =
        Some(Arc::new(|| crate::conversation::NativePolicyHostFacts {
            bash_first_assignment: Some(
                crate::conversation::NativeBashFirstSessionAssignment::Forced,
            ),
            ..Default::default()
        }));
    (orchestrator, permission_gate)
}

async fn current_base_ingress_rows(
    orchestrator: &ConversationOrchestrator,
) -> Vec<PromptIngressAttachment> {
    let mut rows = orchestrator.environment_prompt_ingress_attachments().await;
    if let Some(model) = orchestrator.model_prompt_ingress_attachment().await {
        rows.push(model);
    }
    rows
}

fn bind_test_durable_transcript_target(
    orchestrator: &ConversationOrchestrator,
    cwd: &Path,
    state_root: &Path,
    transcript: &Path,
) {
    std::fs::create_dir_all(state_root).expect("create test durable state root");
    let durable = Arc::new(
        session::jsonl::DurableTranscriptWriter::open(state_root)
            .expect("open test durable transcript writer"),
    );
    let session_id = orchestrator
        .session
        .try_lock()
        .expect("bind durable target before the test turn begins")
        .session_id;
    let writer = orchestrator
        .session_transcript_writer()
        .expect("test orchestrator has a JSONL writer");
    writer.set_durable_lock(durable);
    writer
        .activate_session_target(session_id, transcript.to_owned(), cwd.to_owned())
        .expect("bind pending durable target to the orchestrator session");
}

fn fallback_attachment() -> RequestContextTranscriptAttachment {
    RequestContextTranscriptAttachment {
        message_id: MessageId::new(),
        payload: Utf16JsonProjection::parse(
            r#"{"type":"unknown_command_fallback","commandName":"probe","availableCommandNames":["run"],"availableCommandCount":1}"#,
        )
        .unwrap(),
        rendered: Utf16JsonProjection::parse(
            r#"[{"content":"<system-reminder>\nUnknown slash fallback\n</system-reminder>"}]"#,
        )
        .unwrap(),
        rendered_role: "user",
    }
}

fn staged_system_attachment(kind: &str, text: &str) -> PromptIngressAttachment {
    let message = ConversationMessage::user_meta(
        MessageId::new(),
        format!("<system-reminder>\n{text}\n</system-reminder>"),
    );
    let payload = Utf16JsonProjection::parse(&format!(r#"{{"type":"{kind}"}}"#)).unwrap();
    PromptIngressAttachment {
        rendered: rendered_system_message(&message).unwrap(),
        message,
        payload,
    }
}

async fn append_accepted_ingress_fixture(
    orchestrator: &ConversationOrchestrator,
    row: &PromptIngressAttachment,
) {
    orchestrator
        .append_accepted_prompt_ingress_reminder(row)
        .await;
    orchestrator
        .persist_staged_prompt_ingress_reminder(row)
        .await;
}

fn staged_api_system_tool_change() -> ConversationMessage {
    use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
    use lingxi_core::types::{ApiSystemMessage, ContentBlock};

    ConversationMessage::api_system(
        MessageId::new(),
        ApiSystemMessage {
            content: vec![ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: Source::parse(
                    r#"{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"mcp__srv__tool","description":"source","input_schema":{"type":"object"}}}}"#,
                )
                .unwrap(),
            }],
            output_config: None,
        },
    )
}

fn attachment_types(rows: &[session::JsonlMessage]) -> Vec<String> {
    rows.iter()
        .filter(|row| row.message_type == "attachment")
        .filter_map(|row| {
            row.attachment_projection
                .as_ref()?
                .string_at("/type")
                .map(str::to_owned)
        })
        .collect()
}

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(build());
        })
        .unwrap()
        .join()
        .unwrap();
}

fn mod_registry(host: Arc<hooks::mods::ModHost>) -> Arc<tokio::sync::RwLock<hooks::HookRegistry>> {
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    Arc::new(tokio::sync::RwLock::new(registry))
}

fn request_prompt_context(prompt_id: MessageId) -> PromptContext {
    PromptContext {
        prompt_id: std::sync::Mutex::new(prompt_id),
        rows: context_rows(Vec::new(), None),
        turn_projection: std::sync::Mutex::new(None),
        transcript_attachment: std::sync::Mutex::new(None),
        staged_ingress_attachments: std::sync::Mutex::new(None),
        ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
        next_model_step_ingress_kind: std::sync::Mutex::new(None),
        deferred_tools_projection_facts: std::sync::Mutex::new(None),
    }
}

struct SkillListingFixture;

#[async_trait]
impl SkillListingProvider for SkillListingFixture {
    async fn skill_listing_snapshot(
        &self,
    ) -> Result<SkillListingSnapshot, lingxi_core::settings::SettingsError> {
        Ok(SkillListingSnapshot {
            entries: vec![SkillListingEntry {
                name: "typed-fixture".into(),
                display_name: "typed-fixture".into(),
                user_facing_name: None,
                unqualified_name: None,
                is_synced_skill: false,
                bare_name_reserved: false,
                aliases: Vec::new(),
                source: None,
                plugin_name: None,
                description: "fixture skill".into(),
                when_to_use: None,
                is_bundled_prompt: false,
                is_name_only: false,
                priority: 0.0,
            }],
            budget_settings: Default::default(),
        })
    }
}

struct SkillListingGateTool {
    name: &'static str,
}

#[async_trait]
impl Tool for SkillListingGateTool {
    fn name(&self) -> &str {
        self.name
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<Utf16JsonProjection> =
            once_cell::sync::Lazy::new(|| {
                tool_api::tool_trait::generated_builtin_result(
                    &serde_json::json!({ "type": "object", "properties": {} }),
                )
            });
        &SCHEMA
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        String::new()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        String::new().into()
    }

    async fn call(
        &self,
        _input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

fn orchestrator_with_skill_listing(
    cwd: &Path,
    transcript: &Path,
    filesystem: Arc<dyn FileSystem>,
) -> ConversationOrchestrator {
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(SkillListingGateTool { name: "Skill" }));
    ConversationOrchestrator::new(
        crate::OrchestratorConfig {
            context_rendering: InstructionRendering::Announced,
            ..Default::default()
        },
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    )
    .with_jsonl_writer(Arc::new(JsonlWriter::new(
        transcript.to_owned(),
        filesystem,
    )))
    .with_skill_listing(Arc::new(SkillListingFixture))
}

async fn append_typed_skill_history(orch: &ConversationOrchestrator) -> [MessageId; 3] {
    let header = ConversationMessage::user(MessageId::new(), "/review".to_owned());
    let mut body = ConversationMessage::user_meta(MessageId::new(), "expanded review body".into());
    if let ConversationMessage::User { turn_companion, .. } = &mut body {
        *turn_companion = Some(Some(true));
    }
    let upe = ConversationMessage::user_meta(
        MessageId::new(),
        "UserPromptExpansion hook additional context".into(),
    );
    let ids = [header.id(), body.id(), upe.id()];
    for message in [header, body, upe] {
        orch.session.lock().await.history.push(message.clone());
        orch.persist_message_to_jsonl(&message).await;
    }
    ids
}

async fn prepare_local_skill_ingress(
    orch: &ConversationOrchestrator,
    prompt_id: MessageId,
    expected_ids: [MessageId; 3],
) -> Vec<ConversationMessage> {
    REQUEST_CONTEXT
        .scope(request_prompt_context(prompt_id), async {
            persist_prompt_ingress_after_user(orch, prompt_id, PromptIngressKind::LocalSkill)
                .await
                .unwrap();
            assert!(
                !orch.regular_user_prompt_for_model_step().await,
                "the trusted companion body is explicitly non-regular"
            );
            let mut prepared = orch
                .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
                .await
                .unwrap();
            for id in expected_ids {
                assert_eq!(
                    prepared
                        .snapshot
                        .iter()
                        .filter(|row| row.id() == id)
                        .count(),
                    1,
                    "typed body and UPE history rows remain once in the prepared request"
                );
            }
            assert!(prepared
                .snapshot
                .iter()
                .any(|row| row.text_content().contains("Environment")));
            assert!(prepared.snapshot.iter().any(|row| {
                let text = row.text_content();
                text.contains("typed-fixture") && text.contains("following skills")
            }));
            assert!(!prepared
                .snapshot
                .iter()
                .any(|row| row.text_content().contains("<total_tokens>")));
            orch.persist_prepared_turn_attachments(&mut prepared).await;
            prepared.snapshot
        })
        .await
}

static TOTAL_TOKEN_TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) struct TotalTokenTestEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    mode: Option<std::ffi::OsString>,
    after_user: Option<std::ffi::OsString>,
}

impl TotalTokenTestEnv {
    pub(crate) fn enabled() -> Self {
        let lock = TOTAL_TOKEN_TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mode = std::env::var_os("CLAUDE_CODE_TOTAL_TOKENS_REMINDER");
        let after_user = std::env::var_os("CLAUDE_CODE_TOTAL_TOKENS_REMINDER_AFTER_USER_TURN");
        std::env::set_var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "padded-countdown");
        std::env::set_var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER_AFTER_USER_TURN", "true");
        Self {
            _lock: lock,
            mode,
            after_user,
        }
    }
}

impl Drop for TotalTokenTestEnv {
    fn drop(&mut self) {
        match self.mode.take() {
            Some(value) => std::env::set_var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", value),
            None => std::env::remove_var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER"),
        }
        match self.after_user.take() {
            Some(value) => {
                std::env::set_var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER_AFTER_USER_TURN", value)
            }
            None => std::env::remove_var("CLAUDE_CODE_TOTAL_TOKENS_REMINDER_AFTER_USER_TURN"),
        }
    }
}

#[test]
fn cancelled_prepare_keeps_input_sidecar_and_cold_resume_reuses_context() {
    let _total_token_env = TotalTokenTestEnv::enabled();
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("session.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone());
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );
        let first_prompt_id = MessageId::new();
        let first_prompt = ConversationMessage::user(first_prompt_id, "/probe argument".into());
        orch.session.lock().await.history.push(first_prompt.clone());
        orch.persist_message_to_jsonl(&first_prompt).await;
        let reminder = Utf16JsonProjection::parse(
            r#""<system-reminder>\nUnknown slash fallback\n</system-reminder>""#,
        )
        .unwrap();
        let fallback = fallback_attachment();
        let fallback_id = fallback.message_id;

        // Native records the original prompt, the full common ingress batch,
        // and its typed fallback sidecar before model-engine admission.
        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(first_prompt_id),
                    rows: context_rows(vec![reminder], Some(&fallback)),
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(Some(fallback)),
                    staged_ingress_attachments: std::sync::Mutex::new(None),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
                    next_model_step_ingress_kind: std::sync::Mutex::new(None),
                    deferred_tools_projection_facts: std::sync::Mutex::new(None),
                },
                async {
                    persist_prompt_ingress_after_user(
                        &orch,
                        first_prompt_id,
                        PromptIngressKind::Other,
                    )
                    .await
                    .expect("prompt ingress should persist before preparing the model step");
                    let prepared = orch
                        .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
                        .await
                        .unwrap();
                    let hot_fallback_rows = prepared
                        .snapshot
                        .iter()
                        .filter(|message| message.id() == fallback_id)
                        .collect::<Vec<_>>();
                    assert_eq!(hot_fallback_rows.len(), 1);
                    assert!(matches!(
                        hot_fallback_rows[0],
                        ConversationMessage::User { .. }
                    ));
                    assert!(hot_fallback_rows[0].is_meta());
                    assert!(!prepared.deferred_context_announcement_rows.is_empty());
                    assert!(orch
                        .context_attachment_history(&prepared.context_announcements.messages)
                        .is_empty());
                    let request_types = prepared
                        .context_announcements
                        .messages
                        .iter()
                        .map(|message| message.text_content())
                        .collect::<Vec<_>>();
                    assert!(request_types
                        .iter()
                        .any(|text| text.contains("staged instructions")));
                    drop(prepared);
                },
            )
            .await;

        let after_drop = JsonlReader::new(transcript.clone(), filesystem.clone())
            .read_all()
            .await
            .unwrap();
        let dropped_types = attachment_types(&after_drop);
        let dropped_type_refs = dropped_types.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            dropped_type_refs,
            [
                "environment",
                "model",
                "total_tokens_reminder",
                "unknown_command_fallback"
            ],
            "the fallback collector persists total tokens with a false reanchor bit, not a regular-user budget claim"
        );
        assert_eq!(
            dropped_types
                .iter()
                .filter(|kind| kind.as_str() == "unknown_command_fallback")
                .count(),
            1,
            "prompt-ingress fallback is recorded before model admission"
        );
        assert!(!dropped_types
            .iter()
            .any(|kind| { matches!(kind.as_str(), "instructions" | "session_context" | "date") }));
        let fallback_index = after_drop
            .iter()
            .position(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type"))
                        == Some("unknown_command_fallback")
            })
            .expect("fallback sidecar survives dropped prepare");
        assert!(fallback_index > 0);
        assert_eq!(
            after_drop[fallback_index].parent_uuid.as_deref(),
            Some(after_drop[fallback_index - 1].uuid.as_str()),
            "the fallback sidecar follows the last common ingress row"
        );
        assert_eq!(
            after_drop[fallback_index - 1]
                .attachment_projection
                .as_ref()
                .and_then(|payload| payload.string_at("/type")),
            Some("total_tokens_reminder"),
            "the fallback sidecar follows the last common producer, including its false-reanchor total-token reminder"
        );
        let fallback_uuid = fallback_id.as_uuid().to_string();
        assert_eq!(
            after_drop[fallback_index].uuid, fallback_uuid,
            "the API fallback message and JSONL row share their stable ID"
        );
        assert!(
            orch.session
                .lock()
                .await
                .history
                .iter()
                .any(|message| message.id() == fallback_id),
            "the fallback row stays in live model history after a cancelled request"
        );

        let second_prompt = ConversationMessage::user(MessageId::new(), "next turn".into());
        orch.session
            .lock()
            .await
            .history
            .push(second_prompt.clone());
        orch.persist_message_to_jsonl(&second_prompt).await;
        let mut accepted = orch
            .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
            .await
            .unwrap();
        assert_eq!(
            accepted
                .snapshot
                .iter()
                .filter(|message| message.id() == fallback_id)
                .count(),
            1,
            "the next live turn replays the fallback row once"
        );
        assert!(accepted
            .context_announcements
            .messages
            .iter()
            .any(|message| message.text_content().contains("staged instructions")));
        orch.persist_prepared_turn_attachments(&mut accepted).await;
        drop(accepted);

        let after_accept = JsonlReader::new(transcript.clone(), filesystem.clone())
            .read_all()
            .await
            .unwrap();
        let accepted_types = attachment_types(&after_accept);
        assert_eq!(
            accepted_types
                .iter()
                .filter(|kind| kind.as_str() == "unknown_command_fallback")
                .count(),
            1,
            "the accepted next turn does not retract the earlier ingress sidecar"
        );
        for kind in ["instructions", "session_context", "date"] {
            assert_eq!(
                accepted_types
                    .iter()
                    .filter(|actual| actual.as_str() == kind)
                    .count(),
                1,
                "the next accepted turn emits one {kind} attachment"
            );
        }
        let context_rows = after_accept
            .iter()
            .filter(|row| {
                row.message_type == "attachment"
                    && matches!(
                        row.attachment_projection
                            .as_ref()
                            .and_then(|payload| payload.string_at("/type")),
                        Some("instructions" | "session_context" | "date")
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            context_rows
                .iter()
                .map(|row| {
                    row.attachment_projection
                        .as_ref()
                        .unwrap()
                        .string_at("/type")
                        .unwrap()
                })
                .collect::<Vec<_>>(),
            ["instructions", "session_context", "date"]
        );
        let accepted_fallback_index = after_accept
            .iter()
            .position(|row| row.message_type == "attachment" && row.uuid == fallback_uuid)
            .expect("the prompt-ingress fallback remains in the accepted transcript");
        let second_prompt_uuid = second_prompt.id().as_uuid().to_string();
        let second_prompt_index = after_accept
            .iter()
            .position(|row| row.message_type == "user" && row.uuid == second_prompt_uuid)
            .expect("the next user prompt is recorded before later context attachments");
        let first_context_index = after_accept
            .iter()
            .position(|row| row.uuid == context_rows[0].uuid)
            .expect("the first engine context row is present in the transcript");
        assert!(accepted_fallback_index < second_prompt_index);
        assert!(second_prompt_index < first_context_index);
        let token_reminder_index = after_accept
            .iter()
            .enumerate()
            .skip(second_prompt_index + 1)
            .find(|(_, row)| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type"))
                        == Some("total_tokens_reminder")
            })
            .map(|(index, _)| index)
            .expect("the next regular prompt records its token reminder before shared context");
        assert!(second_prompt_index < token_reminder_index);
        assert!(token_reminder_index < first_context_index);
        assert!(token_reminder_index > 0);
        assert_eq!(
            after_accept[token_reminder_index].parent_uuid.as_deref(),
            Some(after_accept[token_reminder_index - 1].uuid.as_str()),
            "the next turn's ingress reminder extends the existing JSONL chain"
        );
        assert_eq!(
            context_rows[0].parent_uuid.as_deref(),
            Some(after_accept[token_reminder_index].uuid.as_str()),
            "engine-produced context rows follow the next turn's ingress reminder in JSONL"
        );
        for pair in context_rows.windows(2) {
            assert_eq!(
                pair[1].parent_uuid.as_deref(),
                Some(pair[0].uuid.as_str()),
                "shared context attachments extend the durable parent chain"
            );
        }

        let session_id = orch.session.lock().await.session_id;
        let cold = orchestrator(&cwd, &transcript, filesystem.clone());
        *cold.session.lock().await =
            crate::resume::state_from_messages(session_id.as_uuid(), &after_accept);
        cold.restore_resume_runtime_metadata(&after_accept).await;
        assert!(
            cold.session
                .lock()
                .await
                .history
                .iter()
                .any(|message| message.id() == fallback_id),
            "cold resume restores the fallback attachment as its original model row"
        );
        cold.session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(
                MessageId::new(),
                "cold next turn".into(),
            ));
        let cold_prepared = cold
            .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
            .await
            .unwrap();
        let resumed_fallback_rows = cold_prepared
            .snapshot
            .iter()
            .filter(|message| message.id() == fallback_id)
            .collect::<Vec<_>>();
        assert_eq!(resumed_fallback_rows.len(), 1);
        assert!(matches!(
            resumed_fallback_rows[0],
            ConversationMessage::User { .. }
        ));
        assert!(resumed_fallback_rows[0].is_meta());
        assert!(
            cold_prepared.context_announcements.messages.is_empty(),
            "cold resume reuses the durable announcement cursor instead of duplicating rows"
        );
    });
}

#[test]
fn ordinary_regular_prompt_persists_common_ingress_without_fallback_context() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("ordinary.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let response = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        );
        let mut config = crate::OrchestratorConfig {
            context_rendering: InstructionRendering::Announced,
            max_budget_nano_usd: Some(1_000_000_000),
            ..Default::default()
        };
        config.query_source = "test".into();
        let orch = ConversationOrchestrator::new(
            config,
            Arc::new(MockApiClient::new(vec![response])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd,
        )
        .with_jsonl_writer(Arc::new(JsonlWriter::new(
            transcript.clone(),
            filesystem.clone(),
        )));

        orch.run_turn("ordinary user prompt").await.unwrap();

        let rows = JsonlReader::new(transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        assert_eq!(
            &attachment_types(&rows)[..4],
            [
                "environment",
                "model",
                "total_tokens_reminder",
                "budget_usd"
            ],
            "regular user prompts stage the common NAr/rle group without a fallback DTO"
        );
        assert!(!attachment_types(&rows)
            .iter()
            .any(|kind| kind == "unknown_command_fallback"));
    });
}

#[test]
fn complete_ingress_batch_is_selected_once_before_first_append_and_retried_by_identity() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("complete-ingress.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let api = Arc::new(MockApiClient::new(Vec::new()));
        api.set_native_mid_system_supported(true);
        let mut orch = orchestrator_with_api(&cwd, &transcript, filesystem.clone(), api);
        orch.config.query_source = "sdk".into();

        let closing = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: "No response requested.".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        let closing_id = closing.id();
        let prompt = ConversationMessage::user(MessageId::new(), "first SDK prompt".into());
        let prompt_id = prompt.id();
        {
            let mut session = orch.session.lock().await;
            session.history.extend([closing.clone(), prompt.clone()]);
            session
                .native_request_history
                .synthetic_closing_ids
                .insert(closing_id);
        }
        orch.persist_message_to_jsonl(&closing).await;
        orch.persist_message_to_jsonl(&prompt).await;

        let environment = staged_system_attachment("environment", "environment");
        let model = staged_system_attachment("model", "model");
        let total_tokens = staged_system_attachment("total_tokens_reminder", "total tokens");
        let budget = staged_system_attachment("budget_usd", "budget");
        let sandbox = staged_system_attachment("sandbox_instructions", "sandbox");
        let output_style = staged_system_attachment("output_style_instructions", "output style");
        let language = staged_system_attachment("language", "language");
        let deferred = staged_system_attachment("deferred_tools_delta", "deferred tools");
        let agent_listing = staged_system_attachment("agent_listing_delta", "agents");
        let mcp_instructions = staged_system_attachment("mcp_instructions_delta", "instructions");
        let dropped_tools = staged_system_attachment("mcp_dropped_tools_delta", "dropped tools");
        let skill_listing = staged_system_attachment("skill_listing", "skills");
        let changed_file = staged_system_attachment("edited_text_file", "changed file");
        let queued_command = staged_system_attachment("queued_command", "queued command");
        let proactivity = staged_system_attachment("proactivity", "proactivity withdrawal");
        let auto_mode = staged_system_attachment("auto_mode", "auto mode");
        let auto_mode_exit = staged_system_attachment("auto_mode_exit", "auto mode exit");

        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(prompt_id),
                    rows: Vec::new(),
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(None),
                    staged_ingress_attachments: std::sync::Mutex::new(Some(vec![
                        environment.clone(),
                        model.clone(),
                        total_tokens.clone(),
                        budget.clone(),
                    ])),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
                    next_model_step_ingress_kind: std::sync::Mutex::new(None),
                    deferred_tools_projection_facts: std::sync::Mutex::new(None),
                },
                async {
                    orch.append_accepted_prompt_ingress_reminder(&environment)
                        .await;
                    let deferred_producer_runs =
                        Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    deferred_producer_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert!(
                        crate::request_context::cache_prompt_ingress_producer_rows_pending(
                            &orch,
                            "deferred_tools_delta",
                            vec![deferred.clone()],
                        )
                        .await
                        .is_some()
                    );
                    assert!(
                        !crate::request_context::prompt_ingress_producer_is_complete(
                            "deferred_tools_delta"
                        )
                    );
                    let mut completed_deferred = deferred.clone();
                    completed_deferred.message = orch
                        .mod_prompt_attachment(
                            "deferred_tools_delta",
                            deferred.message.clone(),
                            serde_json::json!({"kind":"engine"}),
                        )
                        .await
                        .expect("the deferred row survives its prompt.attachment hook");
                    assert_eq!(completed_deferred.message.id(), deferred.message.id());
                    completed_deferred.rendered =
                        rendered_system_message(&completed_deferred.message).unwrap();
                    crate::request_context::complete_prompt_ingress_producer_rows(
                        &orch,
                        "deferred_tools_delta",
                        vec![completed_deferred.clone()],
                    )
                    .await
                    .expect("deferred post-processing completes the cached producer");

                    let stateful_producer_runs =
                        Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let producers = [
                        ("native_sandbox", vec![sandbox.clone()]),
                        (
                            "native_settings",
                            vec![output_style.clone(), language.clone()],
                        ),
                        (
                            "native_policy",
                            vec![
                                proactivity.clone(),
                                auto_mode.clone(),
                                auto_mode_exit.clone(),
                            ],
                        ),
                        ("agent_listing_delta", vec![agent_listing.clone()]),
                        ("mcp_instructions_delta", vec![mcp_instructions.clone()]),
                        ("mcp_dropped_tools_delta", vec![dropped_tools.clone()]),
                        ("skill_listing", vec![skill_listing.clone()]),
                        ("changed_files", vec![changed_file.clone()]),
                        ("task_notifications", vec![queued_command.clone()]),
                    ];
                    for (producer, expected_rows) in producers {
                        let first_runs = stateful_producer_runs.clone();
                        let rows_to_produce = expected_rows.clone();
                        let first =
                            crate::request_context::prompt_ingress_producer_rows_or_else(
                                &orch,
                                producer,
                                || async move {
                                    first_runs.fetch_add(
                                        1,
                                        std::sync::atomic::Ordering::SeqCst,
                                    );
                                    rows_to_produce
                                },
                            )
                            .await;
                        assert_eq!(
                            first.iter().map(|row| row.message.id()).collect::<Vec<_>>(),
                            expected_rows
                                .iter()
                                .map(|row| row.message.id())
                                .collect::<Vec<_>>()
                        );
                    }

                    let api_system = staged_api_system_tool_change();
                    let api_system_id = api_system.id();
                    let api_system_uuid = api_system_id.as_uuid().to_string();
                    let api_system_producer_runs =
                        Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let api_system_runs = api_system_producer_runs.clone();
                    let first_api_rows = crate::request_context::prompt_ingress_api_system_rows_or_else(
                        &orch,
                        "native_tool_changes",
                        || {
                            api_system_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            vec![api_system.clone()]
                        },
                    )
                    .await;
                    assert_eq!(first_api_rows[0].id(), api_system_id);
                    let cached_api_rows =
                        crate::request_context::prompt_ingress_api_system_rows_or_else(
                            &orch,
                            "native_tool_changes",
                            || panic!("API-system retry must reuse its cached UUID"),
                        )
                        .await;
                    assert_eq!(cached_api_rows[0].id(), api_system_id);

                    let before_append = JsonlReader::new(transcript.clone(), filesystem.clone())
                        .read_all()
                        .await
                        .unwrap();
                    assert!(
                        attachment_types(&before_append).is_empty(),
                        "all ingress rows stay staged until one selector sees the complete batch"
                    );
                    assert!(
                        !before_append
                            .iter()
                            .any(|row| row.uuid == api_system_uuid),
                        "model-only tool changes stay staged until after placement selection"
                    );

                    let cancelled = lingxi_core::host::CancellationToken::new();
                    cancelled.cancel();
                    crate::request_context::finalize_prompt_ingress_batch_with_cancel_for_test(
                        &orch,
                        true,
                        &cancelled,
                    )
                    .await;
                    let after_cancel_before_append =
                        JsonlReader::new(transcript.clone(), filesystem.clone())
                            .read_all()
                            .await
                            .unwrap();
                    assert!(
                        attachment_types(&after_cancel_before_append).is_empty(),
                        "cancellation after selection leaves the first durable append untouched"
                    );
                    assert!(
                        !after_cancel_before_append
                            .iter()
                            .any(|row| row.uuid == api_system_uuid),
                        "cancellation before commit leaves the API-system row undurable"
                    );
                    for producer in [
                        "native_sandbox",
                        "native_settings",
                        "native_policy",
                        "agent_listing_delta",
                        "mcp_instructions_delta",
                        "mcp_dropped_tools_delta",
                        "skill_listing",
                        "changed_files",
                        "task_notifications",
                        "deferred_tools_delta",
                    ] {
                        let _ = crate::request_context::prompt_ingress_producer_rows_or_else(
                            &orch,
                            producer,
                            || async { panic!("a cancelled retry must reuse cached producer rows") },
                        )
                        .await;
                    }
                    assert_eq!(
                        stateful_producer_runs.load(std::sync::atomic::Ordering::SeqCst),
                        9,
                        "cancellation retry does not consume any cached NAr producer twice"
                    );
                    assert_eq!(
                        deferred_producer_runs.load(std::sync::atomic::Ordering::SeqCst),
                        1,
                        "the pending deferred producer result is reused through cancellation"
                    );
                    assert_eq!(
                        api_system_producer_runs.load(std::sync::atomic::Ordering::SeqCst),
                        1,
                        "API-system tool-change state is cached across cancellation retry"
                    );
                    let expected_selected = [
                        &sandbox,
                        &environment,
                        &model,
                        &output_style,
                        &language,
                        &completed_deferred,
                        &agent_listing,
                        &mcp_instructions,
                        &dropped_tools,
                        &skill_listing,
                        &proactivity,
                        &auto_mode,
                    ]
                    .into_iter()
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                    assert_eq!(
                        orch.session
                            .lock()
                            .await
                            .native_request_history
                            .before_closing_line_ids,
                        expected_selected,
                        "the selector completed and cached the full eligible ID set before cancellation"
                    );
                    assert!(
                        !orch
                            .session
                            .lock()
                            .await
                            .native_request_history
                            .before_closing_line_ids
                            .contains(&api_system_id),
                        "api_system tool-change rows are not Native NAr placement candidates"
                    );

                    let append_cancel = lingxi_core::host::CancellationToken::new();
                    let append_gate =
                        Arc::new(crate::request_context::PromptIngressAppendTestGate::default());
                    let finalize =
                        crate::request_context::PROMPT_INGRESS_APPEND_TEST_GATE.scope(
                            append_gate.clone(),
                            crate::turn_loop::await_prepare_with_ingress_commit_boundary(
                                async {
                                    crate::request_context::finalize_prompt_ingress_batch_with_cancel_for_test(
                                        &orch,
                                        true,
                                        &append_cancel,
                                    )
                                    .await;
                                    Ok(())
                                },
                                Some(&append_cancel),
                            ),
                        );
                    tokio::pin!(finalize);
                    tokio::select! {
                        () = append_gate.wait_until_entered() => {
                            append_cancel.cancel();
                            append_gate.resume();
                        }
                        result = &mut finalize => {
                            panic!("finalizer unexpectedly completed before its first durable append pause: {result:?}");
                        }
                    }
                    assert!(finalize.await.is_err(), "cancel is reported after batch completion");
                    assert!(append_cancel.is_cancelled());
                    let after_first_append = JsonlReader::new(transcript.clone(), filesystem.clone())
                        .read_all()
                        .await
                        .unwrap();
                    assert_eq!(
                        after_first_append
                            .iter()
                            .filter(|row| row.uuid == api_system_uuid)
                            .count(),
                        1,
                        "cached API-system changes become durable once with the attachment batch"
                    );
                    let api_system_position = after_first_append
                        .iter()
                        .position(|row| row.uuid == api_system_uuid)
                        .expect("the staged API-system row is durable after placement selection");
                    let first_attachment_position = after_first_append
                        .iter()
                        .position(|row| row.message_type == "attachment")
                        .expect("the typed attachment batch is durable after placement selection");
                    assert!(
                        api_system_position < first_attachment_position,
                        "API-system normalization rows retain their order before NAr attachment writes"
                    );
                    let attachments = after_first_append
                        .iter()
                        .filter(|row| row.message_type == "attachment")
                        .collect::<Vec<_>>();
                    let types = attachment_types(&after_first_append);
                    assert_eq!(
                        types,
                        [
                            "queued_command",
                            "sandbox_instructions",
                            "environment",
                            "model",
                            "output_style_instructions",
                            "language",
                            "deferred_tools_delta",
                            "agent_listing_delta",
                            "mcp_instructions_delta",
                            "mcp_dropped_tools_delta",
                            "edited_text_file",
                            "skill_listing",
                            "proactivity",
                            "auto_mode",
                            "auto_mode_exit",
                            "total_tokens_reminder",
                            "budget_usd",
                        ],
                        "the durable batch follows Native NAr producer order"
                    );
                    let eligible = [
                        "sandbox_instructions",
                        "environment",
                        "model",
                        "output_style_instructions",
                        "language",
                        "deferred_tools_delta",
                        "agent_listing_delta",
                        "mcp_instructions_delta",
                        "mcp_dropped_tools_delta",
                        "skill_listing",
                        "proactivity",
                        "auto_mode",
                    ];
                    for row in &attachments {
                        let kind = row
                            .attachment_projection
                            .as_ref()
                            .and_then(|payload| payload.string_at("/type"))
                            .expect("attachment type is typed");
                        assert_eq!(
                            row.extra.string_at("/placement"),
                            eligible.contains(&kind).then_some("before_closing_line"),
                            "the first append marks exactly current eligible {kind}"
                        );
                    }
                    let ids = attachments
                        .iter()
                        .map(|row| row.uuid.clone())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        orch.transcript.last_jsonl_uuid.lock().await.as_deref(),
                        ids.last().map(String::as_str),
                        "cancellation after the first durable row does not strand the transcript tip"
                    );
                    crate::request_context::finalize_prompt_ingress_batch_for_test(&orch, true)
                        .await;
                    let after_retry = JsonlReader::new(transcript.clone(), filesystem.clone())
                        .read_all()
                        .await
                        .unwrap();
                    assert_eq!(
                        after_retry
                            .iter()
                            .filter(|row| row.message_type == "attachment")
                            .map(|row| row.uuid.clone())
                            .collect::<Vec<_>>(),
                        ids,
                        "retry keeps every attachment identity and does not append duplicates"
                    );
                    assert!(crate::request_context::prompt_ingress_batch_finalized());
                },
            )
            .await;
    });
}

#[test]
fn multiple_staged_environment_rows_keep_their_source_order_in_the_request() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let orch = orchestrator(
            &cwd,
            &root.path().join("session.jsonl"),
            Arc::new(PosixFileSystem::new(root.path().to_owned())),
        );
        let prompt = ConversationMessage::user(MessageId::new(), "prompt".into());
        let first_environment = staged_system_attachment("environment", "full environment");
        let reconciliation = staged_system_attachment("environment", "environment withdrawal");
        let model = staged_system_attachment("model", "model identity");
        let prompt_id = prompt.id();
        {
            let mut session = orch.session.lock().await;
            session.history.push(prompt.clone());
            session.history.push(first_environment.message.clone());
            session.history.push(reconciliation.message.clone());
            session.history.push(model.message.clone());
        }

        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(prompt_id),
                    rows: Vec::new(),
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(None),
                    staged_ingress_attachments: std::sync::Mutex::new(Some(vec![
                        first_environment.clone(),
                        reconciliation.clone(),
                        model.clone(),
                    ])),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch {
                        finalized: true,
                        ..PromptIngressBatch::default()
                    }),
                    next_model_step_ingress_kind: std::sync::Mutex::new(None),
                    deferred_tools_projection_facts: std::sync::Mutex::new(None),
                },
                async {
                    let prepared = orch
                        .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
                        .await
                        .unwrap();
                    let first = prepared
                        .snapshot
                        .iter()
                        .position(|row| row.id() == first_environment.message.id())
                        .expect("full environment row is projected");
                    let second = prepared
                        .snapshot
                        .iter()
                        .position(|row| row.id() == reconciliation.message.id())
                        .expect("environment reconciliation row is projected");
                    let model_index = prepared
                        .snapshot
                        .iter()
                        .position(|row| row.id() == model.message.id())
                        .expect("model row follows environment rows");
                    let prompt = prepared
                        .snapshot
                        .iter()
                        .position(|row| row.id() == prompt_id)
                        .expect("user prompt remains in the outgoing request");
                    assert!(first < second && second < model_index && model_index < prompt);
                },
            )
            .await;
    });
}

#[test]
fn cancelled_fallback_append_finishes_before_catching_up_the_transcript_tip() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("session.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone());
        let prompt_id = MessageId::new();
        let prompt = ConversationMessage::user(prompt_id, "/probe".into());
        let prompt_uuid = prompt_id.as_uuid().to_string();
        orch.session.lock().await.history.push(prompt.clone());
        orch.persist_message_to_jsonl(&prompt).await;

        let fallback = fallback_attachment();
        let fallback_id = fallback.message_id;
        let fallback_uuid = fallback_id.as_uuid().to_string();
        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(prompt_id),
                    rows: context_rows(Vec::new(), Some(&fallback)),
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(Some(fallback)),
                    staged_ingress_attachments: std::sync::Mutex::new(Some(Vec::new())),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
                    next_model_step_ingress_kind: std::sync::Mutex::new(None),
                    deferred_tools_projection_facts: std::sync::Mutex::new(None),
                },
                async {
                    let cancel = lingxi_core::host::CancellationToken::new();
                    let append_gate = Arc::new(
                        crate::request_context::PromptIngressAppendTestGate::default(),
                    );
                    let finalize =
                        crate::request_context::PROMPT_INGRESS_APPEND_TEST_GATE.scope(
                            append_gate.clone(),
                            crate::turn_loop::await_prepare_with_ingress_commit_boundary(
                                async {
                                    crate::request_context::finalize_prompt_ingress_batch_with_cancel_for_test(
                                        &orch,
                                        true,
                                        &cancel,
                                    )
                                    .await;
                                    Ok(())
                                },
                                Some(&cancel),
                            ),
                        );
                    tokio::pin!(finalize);
                    tokio::select! {
                        () = append_gate.wait_until_entered() => {
                            cancel.cancel();
                            append_gate.resume();
                        }
                        result = &mut finalize => {
                            panic!("fallback finalizer unexpectedly completed before its durable writer ack: {result:?}");
                        }
                    }
                    assert!(finalize.await.is_err(), "cancellation is reported after finalization");

                    let rows = JsonlReader::new(transcript.clone(), filesystem.clone())
                        .read_all()
                        .await
                        .unwrap();
                    let fallback_rows = rows
                        .iter()
                        .filter(|row| {
                            row.message_type == "attachment"
                                && row
                                    .attachment_projection
                                    .as_ref()
                                    .and_then(|payload| payload.string_at("/type"))
                                    == Some("unknown_command_fallback")
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(fallback_rows.len(), 1);
                    assert_eq!(fallback_rows[0].uuid, fallback_uuid);
                    assert_eq!(
                        fallback_rows[0].parent_uuid.as_deref(),
                        Some(prompt_uuid.as_str()),
                        "the fallback remains chained to the already durable user row"
                    );
                    assert_eq!(
                        orch.transcript.last_jsonl_uuid.lock().await.as_deref(),
                        Some(fallback_uuid.as_str()),
                        "the writer ack and in-memory transcript tip agree after cancellation"
                    );

                    crate::request_context::finalize_prompt_ingress_batch_for_test(&orch, true)
                        .await;
                    let retried = JsonlReader::new(transcript.clone(), filesystem.clone())
                        .read_all()
                        .await
                        .unwrap();
                    assert_eq!(
                        retried
                            .iter()
                            .filter(|row| {
                                row.message_type == "attachment"
                                    && row
                                        .attachment_projection
                                        .as_ref()
                                        .and_then(|payload| payload.string_at("/type"))
                                        == Some("unknown_command_fallback")
                            })
                            .count(),
                        1,
                        "a finalized fallback append is not duplicated by retry"
                    );
                },
            )
            .await;
    });
}

#[test]
fn prompt_ingress_mods_screen_request_copies_and_re_resolve_after_cold_resume() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_string = cwd.to_string_lossy().into_owned();
        let session_id = lingxi_core::types::SessionId::new();
        let transcript = session::jsonl::session_path(
            root.path(),
            &cwd_string,
            &session_id.as_uuid().to_string(),
        );
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let module = root.path().join("prompt-ingress-mod.js");
        std::fs::write(
            &module,
            r#"let environmentCalls = 0;
            let fallbackCalls = 0;
            export function register(on) {
              on('prompt.attachment', { type: 'environment' }, ($, e, next) => {
                if (e.origin.kind !== 'engine') throw new Error('environment origin is not engine');
                return next({ ...e, text: `environment rewrite ${++environmentCalls}: ${e.text}` });
              });
              on('prompt.attachment', { type: 'unknown_command_fallback' }, ($, e, next) => {
                if (e.origin.kind !== 'engine') throw new Error('fallback origin is not engine');
                return next({ ...e, text: `fallback rewrite ${++fallbackCalls}: ${e.text}` });
              });
              on('prompt.attachment', { type: 'total_tokens_reminder' }, ($, e) => {
                if (e.origin.kind !== 'engine') throw new Error('token origin is not engine');
                return { text: null };
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "prompt-ingress-mod",
            root.path(),
            &module,
            tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
        )
        .await
        .unwrap();
        let config = crate::OrchestratorConfig {
            context_rendering: InstructionRendering::Announced,
            max_budget_nano_usd: Some(1_000_000_000),
            ..Default::default()
        };
        let response = || {
            mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            )
        };
        let api = Arc::new(MockApiClient::new(vec![response(), response()]));
        let orch = ConversationOrchestrator::new(
            config.clone(),
            api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_session_id(session_id)
        .with_jsonl_writer(Arc::new(JsonlWriter::new(
            transcript.clone(),
            filesystem.clone(),
        )))
        .with_hook_registry(mod_registry(host.clone()));

        let fallback_text = "<system-reminder>\nUnknown slash fallback\n</system-reminder>";
        let fallback_context =
            Utf16JsonProjection::parse(&serde_json::to_string(fallback_text).unwrap()).unwrap();
        let result = orch
            .run_turn_with_request_context(
                "/probe fixed argument",
                vec![fallback_context],
                Some(fallback_attachment()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result, TurnOutcome::EndTurn);

        let first_requests = api.captured_msgs().await;
        assert_eq!(first_requests.len(), 1);
        assert!(first_requests[0]
            .iter()
            .any(|message| message.text_content().contains("environment rewrite 1:")));
        assert!(first_requests[0]
            .iter()
            .any(|message| message.text_content().contains("fallback rewrite 1:")));

        // A separate ordinary turn reuses the same in-process Mod answer for
        // the durable attachment identities without exercising tool dispatch.
        orch.run_turn("continue").await.unwrap();
        let requests = api.captured_msgs().await;
        assert_eq!(
            requests.len(),
            2,
            "the warm follow-up turn performs a second request"
        );
        for request in &requests {
            let environment = request
                .iter()
                .find(|message| message.text_content().contains("# Environment"))
                .expect("common environment row reaches each request");
            assert!(environment
                .text_content()
                .contains("environment rewrite 1:"));
            let fallback = request
                .iter()
                .find(|message| message.text_content().contains("Unknown slash fallback"))
                .expect("fallback row reaches each request");
            assert!(fallback.text_content().contains("fallback rewrite 1:"));
            assert!(
                !request
                    .iter()
                    .any(|message| message.text_content().contains("<total_tokens>")),
                "Mod omission applies to the outgoing total-token attachment"
            );
        }

        let first_jsonl = JsonlReader::new(transcript.clone(), filesystem.clone())
            .read_all()
            .await
            .unwrap();
        let original_environment = first_jsonl
            .iter()
            .find(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type"))
                        == Some("environment")
            })
            .expect("original environment attachment is persisted");
        let original_fallback = first_jsonl
            .iter()
            .find(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type"))
                        == Some("unknown_command_fallback")
            })
            .expect("fallback sidecar is persisted despite its request rewrite");
        let original_tokens = first_jsonl
            .iter()
            .find(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type"))
                        == Some("total_tokens_reminder")
            })
            .expect("omitted model row remains persisted");
        assert!(original_environment
            .extra
            .string_at("/rendered/0/content")
            .is_some_and(|text| text.contains("# Environment") && !text.contains("rewrite")));
        assert_eq!(
            original_fallback.extra.string_at("/rendered/0/content"),
            Some(fallback_text),
            "the persisted fallback cache stays original"
        );
        assert!(original_tokens
            .extra
            .string_at("/rendered/0/content")
            .is_some_and(|text| text.contains("<total_tokens>") && !text.contains("rewrite")));

        // A new orchestrator restores descriptors from the original JSONL rows.
        // The same ModHost keeps its script counters, while the new prompt
        // runtime starts with an empty per-message answer cache.
        let cold_api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "cold done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let cold = ConversationOrchestrator::with_resume(
            config,
            session_id.as_uuid(),
            root.path().to_owned(),
            cwd_string,
            filesystem.clone(),
            cold_api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
            Some(Arc::new(JsonlWriter::new(
                transcript.clone(),
                filesystem.clone(),
            ))),
        )
        .await
        .expect("resume the original ingress transcript")
        .with_hook_registry(mod_registry(host));
        cold.run_turn("cold resumed prompt").await.unwrap();
        let cold_request = cold_api.captured_msgs().await;
        assert_eq!(cold_request.len(), 1);
        assert!(cold_request[0]
            .iter()
            .any(|message| { message.text_content().contains("environment rewrite 2:") }));
        assert!(cold_request[0]
            .iter()
            .any(|message| { message.text_content().contains("fallback rewrite 2:") }));
        assert!(!cold_request[0]
            .iter()
            .any(|message| message.text_content().contains("<total_tokens>")));
    });
}

#[test]
fn typed_and_cold_pending_skill_keep_common_ingress_listing_and_upe_without_tokens() {
    run_with_large_stack(|| async {
        let _token_env = TotalTokenTestEnv::enabled();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));

        let warm_transcript = root.path().join("typed-skill.jsonl");
        let warm = orchestrator_with_skill_listing(&cwd, &warm_transcript, filesystem.clone());
        let ids = append_typed_skill_history(&warm).await;
        let warm_snapshot = prepare_local_skill_ingress(&warm, ids[0], ids).await;
        assert!(warm_snapshot
            .iter()
            .any(|row| row.text_content().contains("Environment")));
        assert!(warm_snapshot
            .iter()
            .any(|row| row.text_content().contains("typed-fixture")));
        let warm_rows = JsonlReader::new(warm_transcript.clone(), filesystem.clone())
            .read_all()
            .await
            .unwrap();
        let warm_types = attachment_types(&warm_rows);
        assert_eq!(
            warm_types
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["environment", "model", "skill_listing"],
            "LocalSkill stages the complete non-token ingress group before deferred shared context"
        );
        assert!(
            warm_rows.iter().any(|row| {
                row.message_type == "user" && row.uuid == ids[2].as_uuid().to_string()
            }),
            "the UserPromptExpansion history row remains durable under its original ID"
        );
        assert!(!attachment_types(&warm_rows)
            .iter()
            .any(|kind| kind == "total_tokens_reminder"));

        // A fresh orchestrator reconstructs the pending typed body from JSONL,
        // then performs the same per-ingress NAr staging before its first API step.
        let cold_transcript = root.path().join("cold-pending-skill.jsonl");
        let seed = orchestrator_with_skill_listing(&cwd, &cold_transcript, filesystem.clone());
        let cold_ids = append_typed_skill_history(&seed).await;
        let seed_rows = JsonlReader::new(cold_transcript.clone(), filesystem.clone())
            .read_all()
            .await
            .unwrap();
        let session_id = uuid::Uuid::parse_str(&seed_rows[0].session_id).unwrap();
        let cold = orchestrator_with_skill_listing(&cwd, &cold_transcript, filesystem.clone());
        *cold.session.lock().await = crate::resume::state_from_messages(session_id, &seed_rows);
        cold.restore_resume_runtime_metadata(&seed_rows).await;
        let cold_snapshot = prepare_local_skill_ingress(&cold, cold_ids[0], cold_ids).await;
        for id in cold_ids {
            assert_eq!(
                cold_snapshot.iter().filter(|row| row.id() == id).count(),
                1,
                "cold replay retains each original typed Skill/UPE row once"
            );
        }
        let cold_rows = JsonlReader::new(cold_transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        let cold_types = attachment_types(&cold_rows);
        assert!(cold_types.iter().any(|kind| kind == "environment"));
        assert!(cold_types.iter().any(|kind| kind == "model"));
        assert!(cold_types.iter().any(|kind| kind == "skill_listing"));
        assert!(!cold_types
            .iter()
            .any(|kind| kind == "total_tokens_reminder"));
    });
}

#[test]
fn regular_ingress_has_token_and_later_tool_continuation_uses_false_token_path() {
    run_with_large_stack(|| async {
        let _token_env = TotalTokenTestEnv::enabled();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("regular-tool-continuation.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem);
        let user = ConversationMessage::user(MessageId::new(), "regular user prompt".into());
        let user_id = user.id();
        orch.session.lock().await.history.push(user.clone());
        orch.persist_message_to_jsonl(&user).await;

        REQUEST_CONTEXT
            .scope(
                request_prompt_context(user_id),
                async {
                    persist_prompt_ingress_after_user(
                        &orch,
                        user_id,
                        PromptIngressKind::RegularUser,
                    )
                    .await
                    .unwrap();
                    assert!(orch.regular_user_prompt_for_model_step().await);
                    let mut regular = orch
                        .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
                        .await
                        .unwrap();
                    assert!(regular
                        .turn_reminders
                        .iter()
                        .any(|row| row.text_content().contains("<total_tokens>")));
                    orch.persist_prepared_turn_attachments(&mut regular).await;

                    let tool_use_id = lingxi_core::types::ToolUseId::from("toolu_ingress_continuation");
                    let assistant = ConversationMessage::Assistant {
                        per_turn_effort: None,
                        id: MessageId::new(),
                        content: vec![ContentBlock::ToolUse {
                            id: tool_use_id.clone(),
                            name: "Read".into(),
                            input: Utf16JsonProjection::parse("{}").unwrap(),
                            provider_id: None,
                        }],
                        stop_reason: Some("tool_use".into()),
                        resumable: None,
                    };
                    let tool_result = ConversationMessage::User {
                        api_message_override: None,
                        id: MessageId::new(),
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id,
                            content: "read complete".into(),
                            is_error: Some(false),
                            provider_tool_use_id: None,
                            content_blocks: None,
                        }],
                        is_meta: false,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
                        turn_companion: None,
                        usage_limit_note: None,
                        origin: None,
                        turn_origin: None,
                    };
                    for row in [assistant, tool_result] {
                        orch.session.lock().await.history.push(row.clone());
                        orch.persist_message_to_jsonl(&row).await;
                    }
                    assert!(
                        !orch.regular_user_prompt_for_model_step().await,
                        "tool_result ends the regular-user classifier window"
                    );
                    let continuation = orch
                        .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
                        .await
                        .unwrap();
                    assert!(continuation
                        .turn_reminders
                        .iter()
                        .any(|row| row.text_content().contains("<total_tokens>")),
                        "a finalized ingress batch does not stick to the continuation; false/reanchor-false token collection remains active"
                    );
                },
            )
            .await;
    });
}

#[test]
fn auto_compaction_consumes_pending_prompt_before_nar_finalize_without_resurrection() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("compacted-ingress-finalization.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone())
            .with_compaction(Arc::new(compaction::CompactionOrchestrator::new(1)));
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );

        for index in 0..24 {
            let row = ConversationMessage::user(
                MessageId::new(),
                format!("prior conversation row {index} with enough text to compact"),
            );
            orch.session.lock().await.history.push(row.clone());
            orch.persist_message_to_jsonl(&row).await;
        }
        let prompt = ConversationMessage::user(MessageId::new(), "pending current prompt".into());
        let prompt_id = prompt.id();
        orch.session.lock().await.history.push(prompt.clone());
        orch.persist_message_to_jsonl(&prompt).await;
        let nar_attachment = staged_system_attachment("environment", "NAr survives compaction");
        let nar_id = nar_attachment.message.id();
        let qe_id = MessageId::new();
        let qe_row = Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"fixture","content":["buffered Qe context"]}"#,
        )
        .unwrap();

        REQUEST_CONTEXT
            .scope(request_prompt_context(prompt_id), async {
                // A nonempty current NAr batch and an early Qe history append
                // both remain owned by this request across pre-call compaction.
                REQUEST_CONTEXT
                    .try_with(|context| {
                        *context
                            .staged_ingress_attachments
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(vec![nar_attachment.clone()]);
                    })
                    .unwrap();
                assert!(buffer_native_prompt_expansion_rows(
                    std::slice::from_ref(&qe_row),
                    std::slice::from_ref(&qe_id),
                    true,
                )
                .unwrap());
                assert_eq!(pending_ingress_prompt_id(), Some(prompt_id));

                orch.maybe_compact_before_call(None).await;
                let pending_history = orch.session.lock().await.model_context_history();
                assert!(
                    !pending_history.iter().any(|row| row.id() == prompt_id),
                    "successful ordinary auto compaction consumes the pending user row"
                );
                assert!(pending_history.iter().any(|row| matches!(
                    row,
                    ConversationMessage::System { content, .. }
                        if content.string_at("") == Some("Conversation compacted")
                )));
                assert!(!pending_history.iter().any(|row| {
                    row.text_content().contains("prior conversation row 0")
                }));
                assert!(pending_ingress_prompt_was_compacted());
                assert_eq!(
                    prior_history_for_current_prompt(&orch).await,
                    orch.session.lock().await.history.clone(),
                    "NAr producer history is the post-compact full history when its prompt was consumed"
                );

                crate::request_context::finalize_prompt_ingress_batch_for_test(&orch, false).await;
                assert_eq!(pending_ingress_prompt_id(), None);
                let committed_history = orch.session.lock().await.history.clone();
                assert!(
                    !committed_history.iter().any(|row| row.id() == prompt_id),
                    "NAr finalization must not recreate the summarized user row"
                );
                let nar_index = committed_history
                    .iter()
                    .position(|row| row.id() == nar_id)
                    .expect("accepted NAr row is appended after the compacted history");
                let qe_index = committed_history
                    .iter()
                    .position(|row| row.id() == qe_id)
                    .expect("buffered Qe row is flushed after NAr finalization");
                assert!(nar_index < qe_index, "Qe flush follows the accepted NAr batch");
                let jsonl = JsonlReader::new(transcript.clone(), filesystem.clone())
                    .read_all()
                    .await
                    .unwrap();
                let durable_nar_index = jsonl
                    .iter()
                    .position(|row| {
                        row.message_type == "attachment" && row.uuid == nar_id.as_uuid().to_string()
                    })
                    .expect("accepted NAr row is durable");
                let durable_qe_index = jsonl
                    .iter()
                    .position(|row| {
                        row.message_type == "attachment" && row.uuid == qe_id.as_uuid().to_string()
                    })
                    .expect("buffered Qe row is durable");
                assert!(durable_nar_index < durable_qe_index, "JSONL retains NAr before Qe");

                let tool_use_id = lingxi_core::types::ToolUseId::from("toolu_after_ingress_compact");
                let assistant = ConversationMessage::Assistant {
                    per_turn_effort: None,
                    id: MessageId::new(),
                    content: vec![ContentBlock::ToolUse {
                        id: tool_use_id.clone(),
                        name: "Read".into(),
                        input: Utf16JsonProjection::parse("{}").unwrap(),
                        provider_id: None,
                    }],
                    stop_reason: Some("tool_use".into()),
                    resumable: None,
                };
                let tool_result = ConversationMessage::User {
                    api_message_override: None,
                    id: MessageId::new(),
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id,
                        content: "read complete".into(),
                        is_error: Some(false),
                        provider_tool_use_id: None,
                        content_blocks: None,
                    }],
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                };
                for row in [assistant, tool_result] {
                    orch.session.lock().await.history.push(row.clone());
                    orch.persist_message_to_jsonl(&row).await;
                }

                let continuation = orch
                    .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
                    .await
                    .unwrap();
                assert!(continuation.snapshot.iter().any(|row| matches!(
                    row,
                    ConversationMessage::System { content, .. }
                        if content.string_at("") == Some("Conversation compacted")
                )));
                assert!(
                    !continuation
                        .snapshot
                        .iter()
                        .any(|row| row.id() == prompt_id),
                    "the next model step proceeds without reconstructing the compacted ingress prompt"
                );
            })
            .await;
    });
}

#[test]
fn skipped_auto_compaction_does_not_waive_missing_prompt_finalization_error() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("skipped-ingress-compaction.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem)
            .with_compaction(Arc::new(compaction::CompactionOrchestrator::new(u64::MAX)));
        let prompt = ConversationMessage::user(MessageId::new(), "pending current prompt".into());
        let prompt_id = prompt.id();
        orch.session.lock().await.history.push(prompt.clone());
        orch.persist_message_to_jsonl(&prompt).await;

        REQUEST_CONTEXT
            .scope(request_prompt_context(prompt_id), async {
                REQUEST_CONTEXT
                    .try_with(|context| {
                        *context
                            .staged_ingress_attachments
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
                    })
                    .unwrap();
                orch.maybe_compact_before_call(None).await;
                assert!(!pending_ingress_prompt_was_compacted());
                assert!(orch
                    .session
                    .lock()
                    .await
                    .history
                    .iter()
                    .any(|row| row.id() == prompt_id));

                mark_native_request_assembly_accepted();
                orch.session.lock().await.history.retain(|row| row.id() != prompt_id);
                let error = finalize_prompt_ingress_batch_with_enabled(&orch, false, None)
                    .await
                    .expect_err("an unexplained missing ingress row remains a strict failure");
                assert!(matches!(
                    error,
                    OrchestratorError::Internal(message)
                        if message == "accepted prompt-ingress history row is missing during finalization"
                ));
                assert!(!pending_ingress_prompt_was_compacted());
            })
            .await;
    });
}

#[test]
fn accepted_upe_rows_commit_after_nar_on_early_scope_exit_and_keep_pending_ids() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("session.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone());
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );

        // A finished query's UPE row is already durable history and must stay
        // where it was when the next request's NAr and UPE rows are committed.
        let old_upe = Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"old","content":["finished query"]}"#,
        )
        .unwrap();
        orch.prompt_runtime.begin_native_query();
        orch.append_native_prompt_expansion_history_rows(&[old_upe])
            .await;
        let old_history_prefix = orch.session.lock().await.history.clone();
        let old_upe_id = old_history_prefix.last().unwrap().id();

        orch.prompt_runtime.begin_native_query();
        let prompt_id = MessageId::new();
        let user = ConversationMessage::user(prompt_id, "/probe".into());
        orch.session.lock().await.history.push(user.clone());
        orch.persist_message_to_jsonl(&user).await;

        let upe_rows = vec![
            Utf16JsonProjection::parse(
                r#"{"type":"hook_success","hookEvent":"UserPromptExpansion","hookName":"native","content":"success tail"}"#,
            )
            .unwrap(),
            Utf16JsonProjection::parse(
                r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"native","content":["context tail"]}"#,
            )
            .unwrap(),
        ];
        let upe_ids = vec![MessageId::new(), MessageId::new()];
        let plugin_context = Utf16JsonProjection::parse(r#"["plugin tail"]"#).unwrap();

        let nar_ids = crate::request_context::scope_if_missing(&orch, async {
            crate::request_context::persist_prompt_ingress_after_user(
                &orch,
                prompt_id,
                PromptIngressKind::RegularUser,
            )
            .await
            .map_err(OrchestratorError::from)?;
            let nar_ids = crate::request_context::staged_prompt_ingress_attachments()
                .unwrap()
                .into_iter()
                .map(|row| row.message.id())
                .collect::<Vec<_>>();
            assert!(!nar_ids.is_empty());

            orch.append_native_prompt_expansion_history_rows_once(&upe_rows, &upe_ids)
                .await?;
            // Cold-retry re-entry uses the same pending IDs and source rows.
            orch.append_native_prompt_expansion_history_rows_once(&upe_rows, &upe_ids)
                .await?;
            orch.append_mod_prompt_context(&plugin_context).await;

            let history = orch.session.lock().await.history.clone();
            assert_eq!(
                &history[..old_history_prefix.len()],
                old_history_prefix.as_slice()
            );
            assert!(
                !history
                    .iter()
                    .any(|message| upe_ids.contains(&message.id())),
                "current UPE rows stay buffered until NAr finalization"
            );
            let prompt_uuid = prompt_id.as_uuid().to_string();
            assert_eq!(
                orch.transcript.last_jsonl_uuid.lock().await.as_deref(),
                Some(prompt_uuid.as_str()),
                "buffering does not persist an UPE sidecar before NAr"
            );
            let cancelled = lingxi_core::host::CancellationToken::new();
            cancelled.cancel();
            crate::request_context::mark_native_request_assembly_accepted();
            crate::request_context::finalize_prompt_ingress_batch(&orch, Some(&cancelled)).await?;
            Ok(nar_ids)
        })
        .await
        .unwrap();

        let history = orch.session.lock().await.history.clone();
        let history_ids = history
            .iter()
            .map(ConversationMessage::id)
            .collect::<Vec<_>>();
        let old_position = history_ids.iter().position(|id| *id == old_upe_id).unwrap();
        let nar_positions = nar_ids
            .iter()
            .map(|id| {
                history_ids
                    .iter()
                    .position(|candidate| candidate == id)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let upe_positions = upe_ids
            .iter()
            .map(|id| {
                history_ids
                    .iter()
                    .position(|candidate| candidate == id)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(old_position < nar_positions[0]);
        assert!(nar_positions.iter().max().unwrap() < upe_positions.iter().min().unwrap());
        assert_eq!(
            orch.prompt_runtime.current_prompt_upe_attachment_ids(),
            std::collections::HashSet::from([upe_ids[0], upe_ids[1]])
        );

        let rows = JsonlReader::new(transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        let positions = |uuid: &str| {
            rows.iter()
                .position(|row| row.uuid == uuid)
                .expect("buffered row was appended exactly once")
        };
        let last_nar_position = nar_ids
            .iter()
            .map(|id| positions(&id.as_uuid().to_string()))
            .max()
            .unwrap();
        let first_upe_position = upe_ids
            .iter()
            .map(|id| positions(&id.as_uuid().to_string()))
            .min()
            .unwrap();
        assert!(last_nar_position < first_upe_position);
        assert_eq!(
            upe_ids
                .iter()
                .map(|id| {
                    rows.iter()
                        .filter(|row| row.uuid == id.as_uuid().to_string())
                        .count()
                })
                .collect::<Vec<_>>(),
            [1, 1],
            "retry reuses each pending attachment UUID without duplicate writes"
        );
        let plugin_position = rows
            .iter()
            .position(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/hookEvent"))
                        == Some("UserPromptSubmit")
            })
            .unwrap();
        assert!(*upe_positions.iter().max().unwrap() < plugin_position);
    });
}

#[test]
fn production_prepare_projects_buffered_upe_and_prompt_submit_tail_after_nar() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("upe-tail-request.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let mut orch = orchestrator_with_skill_listing(&cwd, &transcript, filesystem);
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );
        orch.config.query_source = "sdk".into();

        // A finished query's UPE row must remain in the earlier completed group.
        orch.prompt_runtime.begin_native_query();
        let old_user = ConversationMessage::user(MessageId::new(), "finished prompt".into());
        orch.session.lock().await.history.push(old_user.clone());
        orch.persist_message_to_jsonl(&old_user).await;
        let old_upe = Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"old","content":["finished UPE"]}"#,
        )
        .unwrap();
        orch.append_native_prompt_expansion_history_rows(&[old_upe])
            .await;
        let old_upe_id = orch.session.lock().await.history.last().unwrap().id();
        let previous_assistant = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: "finished answer".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        orch.session
            .lock()
            .await
            .history
            .push(previous_assistant.clone());
        orch.persist_message_to_jsonl(&previous_assistant).await;
        orch.prompt_runtime.begin_native_query();

        let prompt_id = MessageId::new();
        let header = ConversationMessage::user(prompt_id, "/review".into());
        let mut body = ConversationMessage::user(MessageId::new(), "expanded review body".into());
        if let ConversationMessage::User { turn_companion, .. } = &mut body {
            *turn_companion = Some(Some(true));
        }
        for message in [&header, &body] {
            orch.session.lock().await.history.push(message.clone());
            orch.persist_message_to_jsonl(message).await;
        }
        let upe_rows = vec![
            Utf16JsonProjection::parse(
                r#"{"type":"hook_success","hookEvent":"UserPromptExpansion","hookName":"fixture","content":"current UPE success"}"#,
            )
            .unwrap(),
            Utf16JsonProjection::parse(
                r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"fixture","content":["current UPE context"]}"#,
            )
            .unwrap(),
        ];
        let upe_ids = vec![MessageId::new(), MessageId::new()];
        let plugin_context =
            Utf16JsonProjection::parse(r#"["current prompt.submit context"]"#).unwrap();

        let (prepared, nar_ids, buffered_submit_ids) =
            crate::request_context::scope_if_missing(&orch, async {
                crate::request_context::persist_prompt_ingress_after_user(
                    &orch,
                    prompt_id,
                    PromptIngressKind::LocalSkill,
                )
                .await
                .map_err(OrchestratorError::from)?;
                orch.append_native_prompt_expansion_history_rows_once(&upe_rows, &upe_ids)
                    .await?;
                orch.append_mod_prompt_context(&plugin_context).await;

                let prepared = orch
                    .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
                    .await?;
                let nar_ids = crate::request_context::staged_prompt_ingress_attachments()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                let buffered_submit_ids =
                    crate::request_context::finalized_buffered_prompt_submit_row_ids();
                Ok::<_, OrchestratorError>((prepared, nar_ids, buffered_submit_ids))
            })
            .await
            .unwrap();

        let plugin_id = {
            let attachments = orch.prompt_runtime.mod_persisted_attachments.lock().await;
            let (id, (kind, source)) = attachments
                .iter()
                .find(|(_, (_, source))| source["event"] == "prompt.submit")
                .expect("buffered prompt.submit append keeps its registered source metadata");
            assert_eq!(kind, "hook_additional_context");
            assert_eq!(source["kind"], "plugin");
            *id
        };
        assert_eq!(
            buffered_submit_ids,
            std::collections::HashSet::from([plugin_id]),
            "the finalized request scope retains the buffered prompt.submit identity"
        );
        assert_eq!(
            orch.prompt_runtime.current_prompt_upe_attachment_ids(),
            std::collections::HashSet::from([upe_ids[0], upe_ids[1]])
        );
        {
            let attachments = orch.prompt_runtime.mod_persisted_attachments.lock().await;
            for id in &upe_ids {
                let (kind, source) = attachments
                    .get(id)
                    .expect("each projected UPE row keeps its typed provenance");
                assert!(kind == "hook_success" || kind == "hook_additional_context");
                assert_eq!(source["event"], "UserPromptExpansion");
            }
            let (_, plugin_source) = attachments
                .get(&plugin_id)
                .expect("prompt.submit provenance remains registered");
            assert_eq!(plugin_source["event"], "prompt.submit");
        }

        let position = |id| {
            prepared
                .snapshot
                .iter()
                .position(|message| message.id() == id)
                .expect("prepared request contains the row")
        };
        for id in &upe_ids {
            assert_eq!(
                prepared
                    .snapshot
                    .iter()
                    .filter(|message| message.id() == *id)
                    .count(),
                1,
                "buffered UPE is projected once without a second history append"
            );
        }
        assert_eq!(
            prepared
                .snapshot
                .iter()
                .filter(|message| message.id() == plugin_id)
                .count(),
            1
        );
        let projected_nar_ids = nar_ids
            .iter()
            .filter(|id| prepared.snapshot.iter().any(|message| message.id() == **id))
            .copied()
            .collect::<Vec<_>>();
        assert!(!projected_nar_ids.is_empty());
        let last_nar = projected_nar_ids
            .iter()
            .map(|id| position(*id))
            .max()
            .unwrap();
        let first_upe = upe_ids.iter().map(|id| position(*id)).min().unwrap();
        let last_upe = upe_ids.iter().map(|id| position(*id)).max().unwrap();
        let plugin_position = position(plugin_id);
        let header_position = position(prompt_id);
        let body_position = position(body.id());
        assert!(last_nar < first_upe, "current NAr rows precede UPE rows");
        assert!(
            last_upe < plugin_position,
            "prompt.submit stays after current UPE"
        );
        assert!(plugin_position < header_position && header_position < body_position);
        assert_eq!(
            prepared
                .snapshot
                .iter()
                .filter(|row| row.id() == old_upe_id)
                .count(),
            1
        );
        assert!(
            position(old_upe_id) < position(previous_assistant.id()),
            "finished UPE history remains in its earlier boundary group"
        );
        assert!(prepared
            .snapshot
            .iter()
            .any(|message| message.text_content().contains("current UPE context")));
        assert!(prepared.snapshot.iter().any(|message| {
            message
                .text_content()
                .contains("current prompt.submit context")
        }));

        let history = orch.session.lock().await.history.clone();
        for id in upe_ids.into_iter().chain([plugin_id]) {
            assert_eq!(
                history.iter().filter(|message| message.id() == id).count(),
                1,
                "request projection does not duplicate durable/live appends"
            );
        }
    });
}

#[test]
fn owner_scope_does_not_commit_blocked_prompt_before_native_nar_assembly() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("blocked.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone());
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );
        let prompt_id = MessageId::new();
        let user = ConversationMessage::user(prompt_id, "/probe".into());
        orch.session.lock().await.history.push(user.clone());
        orch.persist_message_to_jsonl(&user).await;
        let upe_rows = vec![Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"fixture","content":["collected before submit"]}"#,
        )
        .unwrap()];
        let upe_ids = vec![MessageId::new()];
        let nar_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_nar_ids = nar_ids.clone();

        // Native's blocked UserPromptSubmit path returns a local warning with
        // shouldQuery=false before NAr assembly. Model that owner-only EndTurn
        // path without manually calling the batch finalizer.
        let outcome = crate::request_context::scope_if_missing(&orch, async {
            crate::request_context::persist_prompt_ingress_after_user(
                &orch,
                prompt_id,
                PromptIngressKind::RegularUser,
            )
            .await?;
            let staged = crate::request_context::staged_prompt_ingress_attachments()
                .expect("the accepted user body stages an NAr candidate batch");
            *captured_nar_ids.lock().unwrap() = staged.iter().map(|row| row.message.id()).collect();
            orch.append_native_prompt_expansion_history_rows_once(&upe_rows, &upe_ids)
                .await?;
            Ok::<_, OrchestratorError>(TurnOutcome::EndTurn)
        })
        .await
        .unwrap();
        assert_eq!(outcome, TurnOutcome::EndTurn);

        let nar_ids = nar_ids.lock().unwrap().clone();
        assert!(!nar_ids.is_empty());
        let history = orch.session.lock().await.history.clone();
        assert!(
            history.iter().all(|message| !nar_ids.contains(&message.id())
                && !upe_ids.contains(&message.id())),
            "an unassembled blocked request adds neither staged NAr nor UPE rows to live history"
        );
        let rows = JsonlReader::new(transcript.clone(), filesystem.clone())
            .read_all()
            .await
            .unwrap();
        assert!(
            attachment_types(&rows).is_empty(),
            "scope cleanup does not durably commit NAr/UPE rows without the assembly mark"
        );

        // A preparation failure before Native has assembled NAr is the same
        // boundary: the owner must preserve the error and drop staged rows.
        let prompt_id = MessageId::new();
        let user = ConversationMessage::user(prompt_id, "/probe".into());
        orch.session.lock().await.history.push(user.clone());
        orch.persist_message_to_jsonl(&user).await;
        let failed_upe_id = MessageId::new();
        let failed = crate::request_context::scope_if_missing(&orch, async {
            crate::request_context::persist_prompt_ingress_after_user(
                &orch,
                prompt_id,
                PromptIngressKind::RegularUser,
            )
            .await?;
            orch.append_native_prompt_expansion_history_rows_once(
                &[Utf16JsonProjection::parse(
                    r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"fixture","content":["must not commit"]}"#,
                )
                .unwrap()],
                &[failed_upe_id],
            )
            .await?;
            Err::<(), _>(OrchestratorError::Internal(
                "synthetic pre-assembly error".into(),
            ))
        })
        .await;
        assert!(
            matches!(failed, Err(OrchestratorError::Internal(message)) if message == "synthetic pre-assembly error")
        );
        assert!(
            !orch
                .session
                .lock()
                .await
                .history
                .iter()
                .any(|message| message.id() == failed_upe_id),
            "a pre-assembly error leaves the pending UPE row out of live history"
        );
        let rows = JsonlReader::new(transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        assert!(
            attachment_types(&rows).is_empty(),
            "a pre-assembly error does not make staged NAr/UPE rows durable"
        );
    });
}

#[test]
fn owner_scope_commits_accepted_batch_on_early_error_after_nar_assembly() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("accepted-error.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone());
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );
        let prompt_id = MessageId::new();
        let user = ConversationMessage::user(prompt_id, "/probe".into());
        orch.session.lock().await.history.push(user.clone());
        orch.persist_message_to_jsonl(&user).await;
        let upe_rows = vec![Utf16JsonProjection::parse(
            r#"{"type":"hook_success","hookEvent":"UserPromptExpansion","hookName":"fixture","content":"accepted tail"}"#,
        )
        .unwrap()];
        let upe_ids = vec![MessageId::new()];
        let nar_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_nar_ids = nar_ids.clone();

        // Assembly has reached the same boundary as prepare.rs, but the
        // downstream provider path then errors. The scope owner must finish
        // the ordered append even though the future itself returns Err.
        let result = crate::request_context::scope_if_missing(&orch, async {
            crate::request_context::persist_prompt_ingress_after_user(
                &orch,
                prompt_id,
                PromptIngressKind::RegularUser,
            )
            .await?;
            let staged = crate::request_context::staged_prompt_ingress_attachments()
                .expect("the accepted user body stages an NAr candidate batch");
            *captured_nar_ids.lock().unwrap() = staged.iter().map(|row| row.message.id()).collect();
            orch.append_native_prompt_expansion_history_rows_once(&upe_rows, &upe_ids)
                .await?;
            crate::request_context::mark_native_request_assembly_accepted();
            Err::<(), _>(OrchestratorError::Internal(
                "synthetic provider failure".into(),
            ))
        })
        .await;
        assert!(
            matches!(result, Err(OrchestratorError::Internal(message)) if message == "synthetic provider failure")
        );

        let nar_ids = nar_ids.lock().unwrap().clone();
        assert!(!nar_ids.is_empty());
        let rows = JsonlReader::new(transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        let nar_positions = nar_ids
            .iter()
            .map(|id| {
                rows.iter()
                    .position(|row| row.uuid == id.as_uuid().to_string())
                    .expect("accepted NAr row is durably appended by the scope owner")
            })
            .collect::<Vec<_>>();
        let upe_position = rows
            .iter()
            .position(|row| row.uuid == upe_ids[0].as_uuid().to_string())
            .expect("accepted UPE row follows the NAr append");
        assert!(*nar_positions.iter().max().unwrap() < upe_position);
        let history_ids = orch
            .session
            .lock()
            .await
            .history
            .iter()
            .map(ConversationMessage::id)
            .collect::<Vec<_>>();
        assert!(nar_ids.iter().all(|id| history_ids.contains(id)));
        assert!(history_ids.contains(&upe_ids[0]));
    });
}

#[test]
fn resume_restores_only_the_exact_prompts_accepted_nar_run_without_reappending_it() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("resume-nar.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let (orch, permission_gate) =
            native_policy_orchestrator(&cwd, &transcript, filesystem.clone());
        permission_gate
            .set_permission_mode("bypassPermissions")
            .await
            .unwrap();
        bind_test_durable_transcript_target(
            &orch,
            &cwd,
            &root.path().join("durable-state"),
            &transcript,
        );

        let prompt_id = MessageId::new();
        let prompt = ConversationMessage::user(prompt_id, "/skill".into());
        let body_id = MessageId::new();
        let mut body = ConversationMessage::user(body_id, "resume same prompt body".into());
        if let ConversationMessage::User { turn_companion, .. } = &mut body {
            *turn_companion = Some(Some(true));
        }
        let mut recovered = current_base_ingress_rows(&orch).await;
        let auto_mode = orch
            .native_policy_prompt_ingress_attachments(&[])
            .await
            .into_iter()
            .find(|row| row.payload.string_at("/type") == Some("auto_mode"))
            .expect("the real Native policy producer emits the accepted auto-mode row");
        orch.session.lock().await.history.push(prompt.clone());
        orch.persist_message_to_jsonl(&prompt).await;
        orch.session.lock().await.history.push(body.clone());
        orch.persist_message_to_jsonl(&body).await;
        recovered.extend([
            staged_system_attachment("deferred_tools_delta", "accepted deferred delta"),
            staged_system_attachment("deferred_tools_record", "accepted name-only state"),
            auto_mode,
        ]);
        for row in &recovered {
            append_accepted_ingress_fixture(&orch, row).await;
        }
        let native_api_system = staged_api_system_tool_change();
        let native_api_system_id = native_api_system.id();
        orch.session
            .lock()
            .await
            .history
            .insert(2, native_api_system);
        let upe = Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","hookName":"fixture","content":["later prompt hook context"]}"#,
        )
        .unwrap();
        orch.append_native_prompt_expansion_history_rows(&[upe])
            .await;
        let hook_id = orch.session.lock().await.history.last().unwrap().id();
        recovered.sort_by_key(|row| native_ingress_source_order(row));
        let recovered_ids = recovered
            .iter()
            .map(|row| row.message.id())
            .collect::<std::collections::HashSet<_>>();

        REQUEST_CONTEXT
            .scope(request_prompt_context(prompt_id), async {
                persist_prompt_ingress_after_user(&orch, prompt_id, PromptIngressKind::LocalSkill)
                    .await
                    .unwrap();

                let staged = staged_prompt_ingress_attachments().unwrap();
                let staged_ids = staged
                    .iter()
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                assert!(recovered_ids.is_subset(&staged_ids));
                for accepted in &recovered {
                    assert_eq!(
                        staged
                            .iter()
                            .filter(|row| row.payload == accepted.payload)
                            .count(),
                        1,
                        "a real producer does not regenerate an already accepted payload with a new UUID"
                    );
                }
                assert!(!staged_ids.contains(&hook_id));
                assert!(!staged_ids.contains(&body_id));
                assert!(!staged_ids.contains(&native_api_system_id));
                assert!(!prompt_ingress_batch_finalized());
                let append_attempted = REQUEST_CONTEXT
                    .try_with(|context| {
                        context
                            .ingress_batch
                            .lock()
                            .unwrap()
                            .append_attempted_ids
                            .clone()
                    })
                    .unwrap();
                assert!(recovered_ids.is_subset(&append_attempted));

                let prior = prior_history_for_current_prompt(&orch).await;
                let prior_ids = prior
                    .iter()
                    .map(ConversationMessage::id)
                    .collect::<std::collections::HashSet<_>>();
                let auto_mode_id = recovered
                    .iter()
                    .find(|row| row.payload.string_at("/type") == Some("auto_mode"))
                    .unwrap()
                    .message
                    .id();
                assert!(prior_ids.contains(&auto_mode_id));
                assert!(!prior_ids.contains(&hook_id));
                assert!(!prior_ids.contains(&native_api_system_id));

                let policy_rows = orch.native_policy_prompt_ingress_attachments(&prior).await;
                assert!(
                    !policy_rows
                        .iter()
                        .any(|row| row.payload.string_at("/type") == Some("auto_mode")),
                    "the real auto-mode producer sees the accepted row and suppresses a duplicate"
                );

                finalize_prompt_ingress_batch_for_test(&orch, false).await;
                assert!(prompt_ingress_batch_finalized());
            })
            .await;

        let rows = JsonlReader::new(transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        for id in recovered_ids {
            assert_eq!(
                rows.iter()
                    .filter(|row| row.message_type == "attachment"
                        && row.uuid == id.as_uuid().to_string())
                    .count(),
                1,
                "recovered accepted UUIDs are not appended twice"
            );
        }
        assert_eq!(
            rows.iter()
                .filter(|row| {
                    row.uuid == hook_id.as_uuid().to_string()
                        && row
                            .attachment_projection
                            .as_ref()
                            .and_then(|payload| payload.string_at("/type"))
                            == Some("hook_additional_context")
                })
                .count(),
            1,
            "the already durable UserPromptExpansion row is not duplicated"
        );
    });
}

#[test]
fn resume_partial_nar_prefix_keeps_existing_ids_and_runs_missing_producers() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("resume-partial-nar.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let (orch, permission_gate) =
            native_policy_orchestrator(&cwd, &transcript, filesystem.clone());
        permission_gate
            .set_permission_mode("bypassPermissions")
            .await
            .unwrap();
        let prompt_id = MessageId::new();
        let prompt = ConversationMessage::user(prompt_id, "resume partial prompt".into());
        orch.session.lock().await.history.push(prompt.clone());
        orch.persist_message_to_jsonl(&prompt).await;
        let prefix = current_base_ingress_rows(&orch)
            .await
            .into_iter()
            .next()
            .expect("the partial-prefix fixture uses a real base producer row");
        append_accepted_ingress_fixture(&orch, &prefix).await;

        REQUEST_CONTEXT
            .scope(request_prompt_context(prompt_id), async {
                persist_prompt_ingress_after_user(&orch, prompt_id, PromptIngressKind::LocalSkill)
                    .await
                    .unwrap();
                assert!(!prompt_ingress_batch_finalized());
                assert!(prompt_ingress_producer_rows("native_policy").is_none());

                let prior = prior_history_for_current_prompt(&orch).await;
                let produced =
                    prompt_ingress_producer_rows_or_else(&orch, "native_policy", || async {
                        orch.native_policy_prompt_ingress_attachments(&prior).await
                    })
                    .await;
                let missing = produced
                    .iter()
                    .find(|row| row.payload.string_at("/type") == Some("auto_mode"))
                    .expect("the real missing Native policy producer still runs");
                let missing_id = missing.message.id();

                let staged = staged_prompt_ingress_attachments().unwrap();
                let staged_ids = staged
                    .iter()
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                assert!(staged_ids.contains(&prefix.message.id()));
                assert_eq!(
                    staged
                        .iter()
                        .filter(|row| row.payload == prefix.payload)
                        .count(),
                    1,
                    "the real base producer suppresses the accepted prefix"
                );
                assert!(staged_ids.contains(&missing_id));
                assert!(!prompt_ingress_batch_finalized());
                let append_attempted = REQUEST_CONTEXT
                    .try_with(|context| {
                        context
                            .ingress_batch
                            .lock()
                            .unwrap()
                            .append_attempted_ids
                            .clone()
                    })
                    .unwrap();
                assert!(append_attempted.contains(&prefix.message.id()));
                assert!(!append_attempted.contains(&missing_id));
            })
            .await;
    });
}

#[test]
fn a_new_prompt_does_not_adopt_the_previous_prompts_nar_staging() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("new-prompt-nar.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let (orch, permission_gate) = native_policy_orchestrator(&cwd, &transcript, filesystem);
        const FULL_PROMPT_MODEL: &str = "claude-sonnet-4-6";
        orch.session.lock().await.model = FULL_PROMPT_MODEL.to_owned();
        orch.tools
            .set_main_loop_model(Some(FULL_PROMPT_MODEL.to_owned()));
        assert_eq!(
            orch.session.lock().await.model,
            FULL_PROMPT_MODEL,
            "the test session and the Native main-loop model use the same full-prompt model"
        );
        assert_eq!(
            orch.tools.main_loop_model().as_deref(),
            Some(FULL_PROMPT_MODEL)
        );
        assert!(
            !tool_api::dh_simple_system_prompt(Some(FULL_PROMPT_MODEL)),
            "Sonnet 4.6 selects the full system prompt"
        );
        permission_gate
            .set_permission_mode("bypassPermissions")
            .await
            .unwrap();
        assert_eq!(orch.permission_mode().as_deref(), Some("bypassPermissions"));

        let old_prompt_id = MessageId::new();
        let old_prompt = ConversationMessage::user(old_prompt_id, "previous prompt".into());
        orch.session.lock().await.history.push(old_prompt.clone());
        orch.persist_message_to_jsonl(&old_prompt).await;
        let mut previous_rows = current_base_ingress_rows(&orch).await;
        let old_auto_mode = orch
            .native_policy_prompt_ingress_attachments(&[])
            .await
            .into_iter()
            .find(|row| row.payload.string_at("/type") == Some("auto_mode"))
            .expect("the real Native policy producer emits the previous mode");
        assert_eq!(
            old_auto_mode
                .payload
                .at_pointer("/steerOnly")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        previous_rows.push(old_auto_mode.clone());
        for row in &previous_rows {
            append_accepted_ingress_fixture(&orch, row).await;
        }

        let new_prompt_id = MessageId::new();
        let new_prompt = ConversationMessage::user(new_prompt_id, "new prompt".into());
        orch.session.lock().await.history.push(new_prompt.clone());
        orch.persist_message_to_jsonl(&new_prompt).await;
        permission_gate.set_permission_mode("auto").await.unwrap();
        assert_eq!(
            orch.permission_mode().as_deref(),
            Some("auto"),
            "the real policy gate accepted the bypass-to-auto transition"
        );

        REQUEST_CONTEXT
            .scope(request_prompt_context(new_prompt_id), async {
                persist_prompt_ingress_after_user(
                    &orch,
                    new_prompt_id,
                    PromptIngressKind::LocalSkill,
                )
                .await
                .unwrap();
                let before_producer = staged_prompt_ingress_attachments().unwrap();
                assert!(!before_producer
                    .iter()
                    .any(|row| row.message.id() == old_auto_mode.message.id()));
                let prior = prior_history_for_current_prompt(&orch).await;
                assert!(prior
                    .iter()
                    .any(|row| row.id() == old_auto_mode.message.id()));

                let produced =
                    prompt_ingress_producer_rows_or_else(&orch, "native_policy", || async {
                        orch.native_policy_prompt_ingress_attachments(&prior).await
                    })
                    .await;
                let new_auto_mode = produced
                    .iter()
                    .find(|row| row.payload.string_at("/type") == Some("auto_mode"))
                    .expect("the changed current permission mode creates a new auto-mode delta");
                let new_auto_mode_id = new_auto_mode.message.id();
                assert_ne!(new_auto_mode_id, old_auto_mode.message.id());
                assert_eq!(
                    new_auto_mode
                        .payload
                        .at_pointer("/steerOnly")
                        .and_then(|value| value.as_bool()),
                    Some(false)
                );
                let staged_ids = staged_prompt_ingress_attachments()
                    .unwrap()
                    .into_iter()
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                assert!(staged_ids.contains(&new_auto_mode_id));
                assert!(!staged_ids.contains(&old_auto_mode.message.id()));
            })
            .await;
    });
}
