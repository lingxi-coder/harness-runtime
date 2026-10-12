use super::*;
use crate::conversation::ModelCallPath;
use crate::prompt::MemoryFile;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use lingxi_core::host::instructions::InstructionRendering;
use lingxi_core::host::FileSystem;
use memory::lingxi_md::LingxiMdTier;
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::path::Path;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

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
        Arc::new(ToolRegistry::new()),
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

#[test]
fn cancelled_prepare_keeps_input_sidecar_and_cold_resume_reuses_context() {
    run_with_large_stack(|| async {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("session.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let orch = orchestrator(&cwd, &transcript, filesystem.clone());
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
                },
                async {
                    persist_prompt_ingress_after_user(&orch, first_prompt_id, true).await;
                    let prepared = orch
                        .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
                        .await
                        .unwrap();
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
            "prompt ingress rows persist before the typed fallback sidecar"
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
            Some("total_tokens_reminder")
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
            ConversationMessage::User { is_meta: false, .. }
        ));
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
                },
                async {
                    orch.stage_prompt_ingress_reminder(&environment).await;
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
