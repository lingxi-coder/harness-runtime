//! Cross-component regressions for Native prompt provenance at compaction.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use command_api::model::{
    CommandFrontmatter, CommandSource, NativeCommandType, SlashCommand, SlashCommandKind,
};
use command_api::{CommandRegistry, RegistrySlashDispatcher};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::{OrchestratorHandle, SlashCommandDispatcher, SlashDispatchResult};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection as Source, ConversationMessage, MessageId,
};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, text_delta, MockStreamingApiClient,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct SummaryCapture(Mutex<Vec<sidequery::SideQueryRequest>>);

#[async_trait::async_trait]
impl sidequery::SideQueryClient for SummaryCapture {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        let has_evidence = request
            .messages
            .iter()
            .any(|row| row.text_content().contains("END-OF-MCP-EVIDENCE"));
        self.0.lock().unwrap().push(request);
        let summary = if has_evidence {
            "<summary>END-OF-MCP-EVIDENCE retained for the user's task.</summary>"
        } else {
            "<summary>Conversation retained.</summary>"
        };
        Ok(sidequery::SideQueryResponse {
            text: Some(summary.into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn response() -> Vec<llm_runtime::HistoryEvent> {
    vec![
        message_start("answer", "test"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]
}

fn orchestrator(
    summary: Arc<SummaryCapture>,
    api: Arc<MockStreamingApiClient>,
    tools: Arc<tool_api::ToolRegistry>,
    mode: ConversationMode,
) -> Arc<ConversationOrchestrator> {
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new().with_side_query_client(summary, "compact-test".into()),
    );
    ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api,
            tools,
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_conversation_mode(mode)
        .with_cache_safe_slot(slot.clone())
        .with_compaction(Arc::new(
            compaction::CompactionOrchestrator::with_autocompactor(
                compaction::Autocompactor::with_forked_runner(runner, slot),
                100,
            ),
        )),
    )
}

fn pair(result: SlashDispatchResult) -> Vec<orchestrator::QueuedPromptInput> {
    let SlashDispatchResult::RunSkillAsTurn {
        command,
        body,
        hook_messages,
        skill,
    } = result
    else {
        panic!("expected Native prompt pair");
    };
    assert!(command
        .string_at("")
        .unwrap()
        .starts_with("<command-message>"));
    orchestrator::skill_prompt_inputs(command, body, hook_messages, skill)
}

struct FixedSkill;
#[async_trait::async_trait]
impl tool_skill::skill::SkillLoader for FixedSkill {
    async fn load(
        &self,
        _name: &Source,
    ) -> Result<Option<tool_skill::skill::SkillDescriptor>, tool_api::tool_trait::ToolError> {
        Ok(Some(tool_skill::skill::SkillDescriptor {
            name: "review".into(),
            description: "review".into(),
            body: "NEW-REVIEW-INSTRUCTION".into(),
            skill_root: Some(PathBuf::from("/skills/review")),
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn slash_directory_skill_then_real_skill_reinvocation_auto_compacts_only_latest() {
    let summary = Arc::new(SummaryCapture::default());
    let tool_use = vec![
        message_start("skill-call", "test"),
        content_block_start_tool_use(0, lingxi_core::types::ToolUseId::new(), "Skill"),
        input_json_delta(0, r#"{"skill":"alias"}"#),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![
        response(),
        tool_use,
        response(),
        response(),
    ]));
    let ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    let mut tools = tool_api::ToolRegistry::new();
    tools.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
        ctx,
        Arc::new(FixedSkill),
    )));
    let orch = orchestrator(
        summary.clone(),
        api.clone(),
        Arc::new(tools),
        ConversationMode::Code,
    );
    let mut registry = CommandRegistry::new();
    registry.register_command(SlashCommand {
        name: "review".into(),
        aliases: vec!["alias".into()],
        description: "review".into(),
        source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        native_type: Some(NativeCommandType::Prompt),
        skill_root: Some(PathBuf::from("/skills/review")),
        kind: SlashCommandKind::Markdown {
            file_path: "/skills/review/SKILL.md".into(),
            frontmatter: CommandFrontmatter::default(),
            prompt_template: "OLD-REVIEW-INSTRUCTION".into(),
        },
        ..Default::default()
    });
    let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
    orch.run_queued_prompt_batch(
        pair(
            dispatcher
                .dispatch("/alias".into(), CancellationToken::new())
                .await,
        ),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let session_id = orch.current_session_id().await.to_string();
    let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
    let first = compaction::invoked_skills::filter_for_scope(scope);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].name.string_at(""), Some("review"));
    assert!(first[0]
        .content
        .string_at("")
        .unwrap()
        .contains("OLD-REVIEW-INSTRUCTION"));

    orch.run_queued_prompt_batch(
        vec![orchestrator::QueuedPromptInput {
            text: "invoke alias".into(),
            ..Default::default()
        }],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let current = compaction::invoked_skills::filter_for_scope(scope);
    assert_eq!(
        current.len(),
        1,
        "the slash and Skill tool share one canonical activation key"
    );
    assert!(current[0]
        .content
        .string_at("")
        .unwrap()
        .contains("NEW-REVIEW-INSTRUCTION"));
    orch.append_external_history_message(ConversationMessage::user(
        MessageId::new(),
        "filler ".repeat(500),
    ))
    .await;
    orch.run_queued_prompt_batch(
        vec![orchestrator::QueuedPromptInput {
            text: "continue".into(),
            ..Default::default()
        }],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        !summary.0.lock().unwrap().is_empty(),
        "the real automatic compactor ran"
    );
    let calls = api.captured_calls().await;
    let context = calls
        .last()
        .unwrap()
        .messages
        .iter()
        .map(ConversationMessage::text_content)
        .collect::<String>();
    assert!(context.contains("NEW-REVIEW-INSTRUCTION"), "{context}");
    assert!(!context.contains("OLD-REVIEW-INSTRUCTION"), "{context}");
}

#[tokio::test]
async fn long_mcp_prompt_is_summarized_as_evidence_in_a_fixed_chat_session() {
    let summary = Arc::new(SummaryCapture::default());
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![
        response(),
        response(),
        response(),
    ]));
    let orch = orchestrator(
        summary.clone(),
        api.clone(),
        Arc::new(tool_api::ToolRegistry::new()),
        ConversationMode::Chat,
    );
    orch.run_queued_prompt_batch(
        vec![orchestrator::QueuedPromptInput {
            text: "start task".into(),
            ..Default::default()
        }],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let evidence = format!(
        "{}END-OF-MCP-EVIDENCE",
        "substantial-source-evidence ".repeat(1500)
    );
    assert!(evidence.len() / 4 > 5000);
    let connection = lingxi_core::types::McpConnectionId::new();
    let mut registry = CommandRegistry::new();
    for command in command_api::mcp_prompts::mcp_prompt_commands(&[(
        "server".into(),
        connection,
        lingxi_core::host::McpPromptDto {
            name: "evidence".into(),
            description: None,
            arguments: vec![],
        },
    )]) {
        registry.register_command(command);
    }
    let resolver: command_api::dispatcher::McpPromptResolver = Arc::new(move |_, _, _| {
        let evidence = evidence.clone();
        Box::pin(async move {
            Ok(Source::from_serde_value(&serde_json::json!({"messages":[{"role":"user","content":{"type":"text","text":evidence}}]})).unwrap())
        })
    });
    let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
        .with_mcp_prompt_resolver(resolver);
    let inputs = pair(
        dispatcher
            .dispatch("/server evidence".into(), CancellationToken::new())
            .await,
    );
    assert!(inputs[1].skill_identity.is_none());
    orch.run_queued_prompt_batch(inputs, CancellationToken::new())
        .await
        .unwrap();
    let captures = summary.0.lock().unwrap();
    let compact_request = captures.iter().find(|r| r.messages.iter().any(|m| m.text_content().contains("END-OF-MCP-EVIDENCE"))).expect("auto compaction must receive the complete MCP evidence, including the end beyond 5,000 tokens");
    assert!(compact_request
        .messages
        .iter()
        .any(|m| m.text_content().len() / 4 > 5000));
    drop(captures);
    orch.ensure_conversation_mode(ConversationMode::Chat)
        .await
        .unwrap();
    assert!(orch
        .ensure_conversation_mode(ConversationMode::Code)
        .await
        .is_err());
    orch.run_queued_prompt_batch(
        vec![orchestrator::QueuedPromptInput {
            text: "continue task".into(),
            ..Default::default()
        }],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let calls = api.captured_calls().await;
    assert!(
        calls
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|row| row.text_content().contains("END-OF-MCP-EVIDENCE")),
        "the summary survives mode projection"
    );
}
