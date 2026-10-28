use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ContentBlock, DocumentSource, ImageSource, MessageId, ToolUseId};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::schema::JsonlMessage;
use std::sync::Arc;

fn read_rows(path: &std::path::Path) -> Vec<JsonlMessage> {
    std::fs::read_to_string(path)
        .expect("read transcript")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("decode transcript row"))
        .collect()
}

fn generated_source(value: &serde_json::Value) -> Source {
    Source::from_serde_value(value).expect("generated test JSON is valid")
}

async fn logged_inputs(output: &MockOutputStream) -> Vec<serde_json::Value> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text }
                if plugin == "session-append" =>
            {
                text.string_at("")
                    .and_then(|text| text.strip_prefix("append:"))
                    .and_then(|input| serde_json::from_str(input).ok())
            }
            _ => None,
        })
        .collect()
}

async fn build_orchestrator(
    root: &std::path::Path,
    path: &std::path::Path,
    module_source: &str,
) -> (ConversationOrchestrator, Arc<MockOutputStream>) {
    let module = root.join("session-append.js");
    std::fs::write(&module, module_source).expect("write Mod fixture");
    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start Mod host");
    host.load(
        "session-append",
        root,
        &module,
        generated_source(&serde_json::json!({})),
    )
    .await
    .expect("load session.append Mod");
    assert!(host.has_event("session.append"));
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        path.to_path_buf(),
        Arc::new(PosixFileSystem::new(root.to_path_buf())),
    ));
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
    .with_jsonl_writer(writer);
    (orchestrator, output)
}

const REWRITE_MOD: &str = r#"export function register(on) {
  on('session.append', ($, e, next) => {
    $.ui.log(`append:${JSON.stringify(e)}`, { to: 'transcript' });
    const blocks = e.message.content;
    if (e.message.type === 'assistant' && blocks[0]?.type === 'text'
        && blocks[0].text === 'merged-first') {
      return next({ ...e, message: { ...e.message, content: [
        { ...blocks[0], text: 'merged-edited' },
        { ...blocks[1], id: 'forged-tool-use-id', name: 'Forged', input: { forged: true } },
        { type: 'text', text: 'merged-added' },
        blocks[3], blocks[4]
      ] } });
    }
    if (e.message.type === 'assistant' && blocks[0]?.type === 'text'
        && blocks[0].text === 'streamed-text') {
      return next({ ...e, message: { ...e.message, content: [
        { ...blocks[0], text: 'streamed-edited' }
      ] } });
    }
    if (e.message.type === 'assistant' && blocks[0]?.type === 'tool_use') {
      return next({ ...e, message: { ...e.message, content: [
        { ...blocks[0], id: 'forged-per-block-id', name: 'ForgedPerBlock', input: { forged: true } }
      ] } });
    }
    if (e.message.type === 'user' && blocks[0]?.type === 'tool_result') {
      return next({ ...e, message: { ...e.message, content: [
        { ...blocks[0], content: 'rewritten tool output', is_error: true }
      ] } });
    }
    if (e.message.type === 'system' && blocks[0]?.type === 'text'
        && blocks[0].text === 'Conversation compacted') {
      return next({ ...e, message: { ...e.message, content: [
        { ...blocks[0], text: 'rewritten compact boundary' }
      ] } });
    }
    if (blocks[0]?.type === 'text' && blocks[0].text === 'throw-original') {
      throw new Error('keep the original transcript row');
    }
    return next(e);
  });
}"#;

#[tokio::test]
async fn merged_session_append_rewrites_content_without_forging_tool_identity() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("session.jsonl");
    let (orchestrator, output) = build_orchestrator(root.path(), &path, REWRITE_MOD).await;

    let tool_use_id = ToolUseId::new();
    let image = ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
        },
    };
    let document = ContentBlock::Document {
        source: DocumentSource::Base64 {
            media_type: "application/pdf".into(),
            data: "ZG9jdW1lbnQ=".into(),
        },
    };
    let assistant = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![
            ContentBlock::Text {
                text: "merged-first".into(),
                citations: None,
            },
            ContentBlock::ToolUse {
                id: tool_use_id.clone(),
                name: "Read".into(),
                input: generated_source(&serde_json::json!({"file_path":"source.txt"})),
                provider_id: Some("toolu_source".into()),
            },
            ContentBlock::Text {
                text: "remove this text".into(),
                citations: None,
            },
            image.clone(),
            document.clone(),
        ],
        stop_reason: Some("tool_use".into()),
        resumable: None,
    };
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(assistant.clone());
    orchestrator
        .persist_assistant_merged(&assistant, None, None, None)
        .await;
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 1, "session.append observer must have run");
    assert_eq!(inputs[0]["message"]["type"], "assistant");
    assert_eq!(inputs[0]["message"]["role"], "assistant");
    assert!(inputs[0]["message"].get("id").is_none());
    assert_eq!(inputs[0]["message"]["content"][0]["text"], "merged-first");
    assert_eq!(inputs[0]["door"], "response");
    assert_eq!(inputs[0]["origin"]["kind"], "model");

    let expected_tool = ContentBlock::ToolUse {
        id: tool_use_id.clone(),
        name: "Read".into(),
        input: generated_source(&serde_json::json!({"file_path":"source.txt"})),
        provider_id: Some("toolu_source".into()),
    };
    let expected = vec![
        ContentBlock::Text {
            text: "merged-edited".into(),
            citations: Some(None),
        },
        ContentBlock::Text {
            text: "merged-added".into(),
            citations: Some(None),
        },
        image.clone(),
        document.clone(),
        expected_tool.clone(),
    ];
    let history = orchestrator.session.lock().await.history.clone();
    let ConversationMessage::Assistant {
        content: history_content,
        ..
    } = &history[0]
    else {
        panic!("assistant row remains an assistant message");
    };
    assert_eq!(history_content, &expected);

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(inputs[0]["uuid"], rows[0].uuid);
    let disk_content = rows[0]
        .message
        .subprojection("/content")
        .expect("assistant content projection");
    assert_eq!(disk_content.string_at("/0/text"), Some("merged-edited"));
    assert_eq!(disk_content.string_at("/1/text"), Some("merged-added"));
    assert!(disk_content
        .subprojection("/2")
        .unwrap()
        .semantically_eq(&generated_source(&serde_json::to_value(&image).unwrap())));
    assert!(disk_content
        .subprojection("/3")
        .unwrap()
        .semantically_eq(&generated_source(&serde_json::to_value(&document).unwrap())));
    let tool_use_wire_id = tool_use_id.to_string();
    assert_eq!(
        disk_content.string_at("/4/id"),
        Some(tool_use_wire_id.as_str())
    );
    assert_eq!(
        disk_content.string_at("/4/provider_id"),
        Some("toolu_source")
    );
    assert_eq!(disk_content.string_at("/4/name"), Some("Read"));
    assert!(disk_content
        .subprojection("/4/input")
        .unwrap()
        .semantically_eq(&generated_source(
            &serde_json::json!({"file_path":"source.txt"})
        )));
    assert!(
        (0..disk_content.array_len("").expect("content array"))
            .all(|index| disk_content.string_at(&format!("/{index}/text"))
                != Some("remove this text"))
    );

    let result = ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: "original tool output".into(),
            is_error: Some(false),
            provider_tool_use_id: Some("toolu_source".into()),
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
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(result.clone());
    orchestrator.persist_message_to_jsonl(&result).await;

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 2);
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[1]["door"], "tool-result");
    assert_eq!(inputs[1]["origin"]["kind"], "tool");
    assert_eq!(inputs[1]["origin"]["tool"], "Read");
    assert_eq!(inputs[1]["uuid"], rows[1].uuid);
    assert_eq!(
        rows[1].message.string_at("/content/0/type"),
        Some("tool_result")
    );
    assert_eq!(
        rows[1].message.string_at("/content/0/tool_use_id"),
        Some(tool_use_id.as_str())
    );
    assert_eq!(
        rows[1].message.string_at("/content/0/provider_tool_use_id"),
        Some("toolu_source")
    );
    assert_eq!(
        rows[1].message.string_at("/content/0/content"),
        Some("rewritten tool output")
    );
    assert_eq!(
        rows[1]
            .message
            .at_pointer("/content/0/is_error")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    let history = orchestrator.session.lock().await.history.clone();
    let ConversationMessage::User {
        content: history_result_content,
        ..
    } = &history[1]
    else {
        panic!("tool result row remains a user message");
    };
    let ContentBlock::ToolResult {
        tool_use_id: history_tool_id,
        content,
        is_error,
        provider_tool_use_id,
        ..
    } = &history_result_content[0]
    else {
        panic!("tool result row remains a tool result");
    };
    assert_eq!(history_tool_id, &tool_use_id);
    assert_eq!(content.string_at(""), Some("rewritten tool output"));
    assert_eq!(*is_error, Some(true));
    assert_eq!(provider_tool_use_id.as_deref(), Some("toolu_source"));

    let throwing = ConversationMessage::user(MessageId::new(), "throw-original".into());
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(throwing.clone());
    orchestrator.persist_message_to_jsonl(&throwing).await;
    let rows = read_rows(&path);
    assert_eq!(rows.len(), 3);
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[2]["uuid"], rows[2].uuid);
    assert_eq!(
        rows[2].message.string_at("/content/0/text"),
        Some("throw-original")
    );
    let history = orchestrator.session.lock().await.history.clone();
    assert_eq!(history[2], throwing);
}

#[tokio::test]
async fn per_block_session_append_updates_merged_history_and_jsonl_rows() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("session.jsonl");
    let (orchestrator, output) = build_orchestrator(root.path(), &path, REWRITE_MOD).await;
    let tool_use_id = ToolUseId::new();
    let assistant = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![
            ContentBlock::Text {
                text: "streamed-text".into(),
                citations: None,
            },
            ContentBlock::ToolUse {
                id: tool_use_id.clone(),
                name: "Bash".into(),
                input: generated_source(&serde_json::json!({"command":"pwd"})),
                provider_id: Some("toolu_streamed".into()),
            },
        ],
        stop_reason: Some("tool_use".into()),
        resumable: None,
    };
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(assistant.clone());

    let tool_lines = orchestrator
        .persist_assistant_per_block(&assistant, None, None, None)
        .await;
    assert!(tool_lines.contains_key(&tool_use_id));
    let history = orchestrator.session.lock().await.history.clone();
    let ConversationMessage::Assistant { content, .. } = &history[0] else {
        panic!("assistant row remains an assistant message");
    };
    assert_eq!(
        content[0],
        ContentBlock::Text {
            text: "streamed-edited".into(),
            citations: Some(None)
        }
    );
    assert_eq!(
        content[1],
        ContentBlock::ToolUse {
            id: tool_use_id.clone(),
            name: "Bash".into(),
            input: generated_source(&serde_json::json!({"command":"pwd"})),
            provider_id: Some("toolu_streamed".into()),
        }
    );

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 2);
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["uuid"], rows[0].uuid);
    assert_eq!(inputs[1]["uuid"], rows[1].uuid);
    assert_eq!(
        rows[0].message.string_at("/content/0/text"),
        Some("streamed-edited")
    );
    assert_eq!(
        rows[1].message.string_at("/content/0/id"),
        Some(tool_use_id.as_str())
    );
    assert_eq!(
        rows[1].message.string_at("/content/0/provider_id"),
        Some("toolu_streamed")
    );
    assert_eq!(rows[1].message.string_at("/content/0/name"), Some("Bash"));
    assert_eq!(
        rows[1].message.subprojection("/content/0/input").unwrap(),
        generated_source(&serde_json::json!({"command":"pwd"}))
    );
}

#[tokio::test]
async fn duplicate_tool_use_anchors_replay_each_accepted_text_overlay() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("session.jsonl");
    let module = r#"export function register(on) {
  on('session.append', ($, e, next) => {
    $.ui.log(`append:${JSON.stringify(e)}`, { to: 'transcript' });
    const id = e.message.content[0].id;
    return next({ ...e, message: { ...e.message, content: [
      { type: 'text', text: 'prefix' },
      { type: 'tool_use', id, name: 'Changed', input: { value: 1 } },
      { type: 'text', text: 'suffix' },
    ] } });
  });
}"#;
    let (orchestrator, output) = build_orchestrator(root.path(), &path, module).await;
    let duplicate_id = ToolUseId::new();
    let assistant = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![
            ContentBlock::ToolUse {
                id: duplicate_id.clone(),
                name: "Bash".into(),
                input: generated_source(&serde_json::json!({"command":"first"})),
                provider_id: None,
            },
            ContentBlock::ToolUse {
                id: duplicate_id.clone(),
                name: "Read".into(),
                input: generated_source(&serde_json::json!({"file":"second"})),
                provider_id: None,
            },
        ],
        stop_reason: Some("tool_use".into()),
        resumable: None,
    };
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(assistant.clone());

    let accepted = orchestrator
        .persist_assistant_merged(&assistant, None, None, None)
        .await;
    let expected = vec![
        ContentBlock::Text {
            text: "prefix".into(),
            citations: Some(None),
        },
        ContentBlock::ToolUse {
            id: duplicate_id.clone(),
            name: "Bash".into(),
            input: generated_source(&serde_json::json!({"command":"first"})),
            provider_id: None,
        },
        ContentBlock::Text {
            text: "suffix".into(),
            citations: Some(None),
        },
        ContentBlock::ToolUse {
            id: duplicate_id,
            name: "Read".into(),
            input: generated_source(&serde_json::json!({"file":"second"})),
            provider_id: None,
        },
        ContentBlock::Text {
            text: "suffix".into(),
            citations: Some(None),
        },
    ];
    let ConversationMessage::Assistant { content, .. } = &accepted else {
        panic!("accepted session row remains an assistant");
    };
    assert_eq!(
        content, &expected,
        "the same anchor overlay replays per source occurrence"
    );
    assert_eq!(orchestrator.session.lock().await.history[0], accepted);

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message.array_len("/content"), Some(5));
    assert_eq!(rows[0].message.string_at("/content/0/text"), Some("prefix"));
    assert_eq!(rows[0].message.string_at("/content/1/name"), Some("Bash"));
    assert_eq!(rows[0].message.string_at("/content/2/text"), Some("suffix"));
    assert_eq!(rows[0].message.string_at("/content/3/name"), Some("Read"));
    assert_eq!(rows[0].message.string_at("/content/4/text"), Some("suffix"));
    assert_eq!(logged_inputs(&output).await.len(), 1);
}

#[tokio::test]
async fn compact_boundary_session_append_keeps_metadata_and_rewrites_disk_and_history_together() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("session.jsonl");
    let (orchestrator, output) = build_orchestrator(root.path(), &path, REWRITE_MOD).await;
    let marker = ConversationMessage::System {
        api_system: None,
        id: MessageId::new(),
        content: "Conversation compacted".into(),
        subtype: Some("compact_boundary".into()),
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: None,
    };

    orchestrator
        .append_external_compact_boundary(
            marker,
            generated_source(&serde_json::json!({"trigger":"manual","preTokens":128})),
        )
        .await;

    let history = orchestrator.session.lock().await.history.clone();
    let ConversationMessage::System {
        content,
        subtype,
        compact_metadata,
        ..
    } = &history[0]
    else {
        panic!("compact boundary remains a system message");
    };
    assert_eq!(content.string_at(""), Some("rewritten compact boundary"));
    assert_eq!(subtype.as_deref(), Some("compact_boundary"));
    assert_eq!(compact_metadata.as_ref().unwrap().pre_tokens, 128);

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 1);
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["message"]["name"], "compact_boundary");
    assert_eq!(inputs[0]["door"], "compaction");
    assert_eq!(inputs[0]["uuid"], rows[0].uuid);
    assert_eq!(
        rows[0].extra.string_at("/content"),
        Some("rewritten compact boundary")
    );
    assert_eq!(
        rows[0].extra.string_at("/subtype"),
        Some("compact_boundary")
    );
    assert_eq!(
        rows[0].extra.string_at("/compactMetadata/trigger"),
        Some("manual")
    );
    assert_eq!(
        rows[0].extra.number_at("/compactMetadata/preTokens"),
        Some(128.0)
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn skill_companion_registers_only_the_accepted_durable_body_and_cold_restores_it() {
    let _guard = compaction::invoked_skills::TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    compaction::invoked_skills::reset_for_test();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let rewrite = r#"export function register(on) {
      on('session.append', ($, e, next) => {
        const blocks = e.message.content;
        if (e.message.type === 'user' && blocks[0]?.text === 'ORIGINAL-X') {
          return next({ ...e, message: { ...e.message, content: [{ ...blocks[0], text: 'ACCEPTED-' }, { ...blocks[0], text: 'Y' }] } });
        }
        return next(e);
      });
    }"#;
    struct CompactClient;
    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for CompactClient {
        async fn query(&self, _: sidequery::SideQueryRequest) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            Ok(sidequery::SideQueryResponse { text: Some("<summary>task context</summary>".into()), structured: None, tool_calls: Vec::new(), usage: cost::Usage::default(), stop_reason: Some("end_turn".into()), retry_count: 0 })
        }
    }
    let (orch, _) = build_orchestrator(root.path(), &path, rewrite).await;
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(sidequery::ForkedAgentRunner::new().with_side_query_client(Arc::new(CompactClient), "compact-test".into()));
    let orch = orch.with_cache_safe_slot(slot.clone()).with_compaction(Arc::new(
        compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot), 100,
        ),
    ));
    let session_id = orch.session.lock().await.session_id;
    let scoped_id = session_id.to_string();
    let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped_id), None);
    let input = crate::skill_prompt_inputs(
        "<command-message>review</command-message>\n<command-name>/review</command-name>".into(),
        "ORIGINAL-X".into(), Vec::new(),
        Some(lingxi_core::host::SlashSkillIdentity { name: "review".into(), path: "/skills/review".into() }),
    ).remove(1);
    let id = MessageId::new();
    let mut message = ConversationMessage::user_meta(id, "ORIGINAL-X".into());
    orch.stamp_skill_prompt_activation(&input, &mut message).await;
    assert!(compaction::invoked_skills::filter_for_scope(scope).is_empty(), "stamping alone cannot activate a Skill");
    orch.admit_skill_prompt_message(&input, &message).await.unwrap();
    let live = compaction::invoked_skills::filter_for_scope(scope);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].content.string_at(""), Some("ACCEPTED-Y"));
    assert_eq!(live[0].path, std::path::PathBuf::from("/skills/review"));
    let restored = compaction::restore_post_compact_skills(live, &[]);
    assert_eq!(restored[0].content.string_at(""), Some("ACCEPTED-Y"));
    assert!(!std::fs::read_to_string(&path).unwrap().contains("ORIGINAL-X"));

    orch.append_external_history_message(ConversationMessage::user(MessageId::new(), "task context ".repeat(300))).await;
    orch.append_external_history_message(ConversationMessage::Assistant {
        per_turn_effort: None, id: MessageId::new(), content: vec![ContentBlock::Text { text: "answer".into(), citations: None }], stop_reason: Some("end_turn".into()), resumable: None,
    }).await;
    let model = orch.session.lock().await.model.clone();
    orch.save_cache_safe_params(None, &model, &[]).await.unwrap();
    orch.maybe_compact_before_call(None).await;
    let compacted = orch.session.lock().await.model_context_history().iter().map(ConversationMessage::text_content).collect::<String>();
    assert!(compacted.contains("ACCEPTED-Y"), "auto compaction must restore the accepted body");
    assert!(!compacted.contains("ORIGINAL-X"));

    // Rebuild from the exact captured transcript, as a cold host does.
    let source = std::fs::read_to_string(&path).unwrap();
    let loaded = session::jsonl::route_lines(&source);
    let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &session_id.as_uuid().to_string());
    compaction::invoked_skills::reset_for_test();
    let cold = ConversationOrchestrator::new(
        OrchestratorConfig::default(), Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()), noop_hook_executor(),
        Arc::new(NoOpPermissionGate), Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()), root.path().to_path_buf(),
    ).with_session_id(session_id);
    cold.restore_captured_session_history(session_id.as_uuid(), &messages, &loaded.messages_in_order,
        false, permission::PermissionMode::Default, None, &source).await.unwrap();
    let cold_rows = compaction::invoked_skills::filter_for_scope(scope);
    assert_eq!(cold_rows.len(), 1);
    assert_eq!(cold_rows[0].content.string_at(""), Some("ACCEPTED-Y"));
    assert_eq!(cold_rows[0].path, std::path::PathBuf::from("/skills/review"));
    compaction::invoked_skills::reset_for_test();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn failed_skill_append_does_not_activate_or_publish_body() {
    let _guard = compaction::invoked_skills::TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    compaction::invoked_skills::reset_for_test();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("unwritable.jsonl");
    std::fs::create_dir(&path).unwrap();
    let (orch, _) = build_orchestrator(root.path(), &path, "export function register(on) { on('session.append', ($, e, next) => next(e)); }").await;
    let scoped_id = orch.session.lock().await.session_id.to_string();
    let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped_id), None);
    let prior_id = MessageId::new();
    compaction::invoked_skills::register_activation_with_message(
        &Source::from("review"), std::path::Path::new("/skills/review"),
        &Source::from("previous accepted body"), None, scope, prior_id,
    );
    let input = crate::skill_prompt_inputs("/review".into(), "body".into(), Vec::new(),
        Some(lingxi_core::host::SlashSkillIdentity { name: "review".into(), path: "/skills/review".into() })).remove(1);
    let mut message = ConversationMessage::user_meta(MessageId::new(), "body".into());
    orch.stamp_skill_prompt_activation(&input, &mut message).await;
    assert!(orch.admit_skill_prompt_message(&input, &message).await.is_err());
    let effective = compaction::invoked_skills::filter_for_scope(scope);
    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0].content.string_at(""), Some("previous accepted body"));
    assert!(orch.session.lock().await.history.is_empty());
    compaction::invoked_skills::reset_for_test();
}

struct FixedReviewSkill;
#[async_trait::async_trait]
impl tool_skill::skill::SkillLoader for FixedReviewSkill {
    async fn load(&self, _: &Source) -> Result<Option<tool_skill::skill::SkillDescriptor>, tool_api::tool_trait::ToolError> {
        Ok(Some(tool_skill::skill::SkillDescriptor {
            name: "review".into(), description: "review".into(), body: "TOOL-X".into(),
            skill_root: Some("/skills/review".into()), ..Default::default()
        }))
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn skill_tool_reinvocation_commits_only_accepted_body_under_publication_fence() {
    use tool_api::tool_trait::Tool;
    struct CompactClient;
    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for CompactClient {
        async fn query(&self, _: sidequery::SideQueryRequest) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            Ok(sidequery::SideQueryResponse { text: Some("<summary>task context</summary>".into()), structured: None, tool_calls: Vec::new(), usage: cost::Usage::default(), stop_reason: Some("end_turn".into()), retry_count: 0 })
        }
    }
    let _guard = compaction::invoked_skills::TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for streaming in [false, true] {
        compaction::invoked_skills::reset_for_test();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let rewrite = r#"export function register(on) {
          on('session.append', ($, e, next) => {
            const blocks = e.message.content;
            if (e.message.type === 'user' && blocks[0]?.text?.includes('TOOL-X')) {
              return next({ ...e, message: { ...e.message, content: [
                { ...blocks[0], text: 'TOOL-' }, { ...blocks[0], text: 'Y' }
              ] } });
            }
            return next(e);
          });
        }"#;
        let (orch, _) = build_orchestrator(root.path(), &path, rewrite).await;
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(sidequery::ForkedAgentRunner::new().with_side_query_client(Arc::new(CompactClient), "compact-test".into()));
        let orch = ConversationOrchestrator::into_shared(
            orch.with_cache_safe_slot(slot.clone()).with_compaction(Arc::new(
                compaction::CompactionOrchestrator::with_autocompactor(compaction::Autocompactor::with_forked_runner(runner, slot), 100),
            )),
        );
        let sid = orch.session.lock().await.session_id;
        let session_text = sid.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_text), None);
        let input = crate::skill_prompt_inputs(
            "<command-message>review</command-message>\n<command-name>/review</command-name>".into(),
            "SLASH-I1".into(), Vec::new(), Some(lingxi_core::host::SlashSkillIdentity { name: "review".into(), path: "/skills/review".into() }),
        ).remove(1);
        let mut old = ConversationMessage::user_meta(MessageId::new(), "SLASH-I1".into());
        orch.stamp_skill_prompt_activation(&input, &mut old).await;
        orch.admit_skill_prompt_message(&input, &old).await.unwrap();
        let old_id = old.id();
        let tool = tool_skill::SkillTool::with_loader(
            tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput { stdout: String::new(), stderr: String::new(), exit_code: 0, timed_out: false }),
            Arc::new(FixedReviewSkill),
        );
        let mut ctx = tool_api::test_support::fresh_ctx();
        ctx.session = Some(orch.session());
        let result = tool.call(Source::parse(r#"{"skill":"alias"}"#).unwrap(), ctx, tool_api::test_support::fresh_tx()).await.unwrap();
        let message = &result.new_messages[0];
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope)[0].content.string_at(""), Some("SLASH-I1"), "tool execution alone must not retire the slash activation");
        let token = tokio_util::sync::CancellationToken::new();
        let fence = Arc::new(crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(token.clone(), Arc::new(tokio::sync::Mutex::new(()))));
        token.cancel();
        let id = ToolUseId::new();
        let accepted = if streaming {
            let stored = orch.append_streamed_query_row(message, Some(fence.clone())).await;
            orch.persist_preappended_stream_row(&stored, None, "2026-10-28T00:00:00.000Z", Some(fence), Some(id)).await
        } else {
            orch.append_committed_tool_skill(message, id, Some(fence)).await
        };
        assert!(!accepted);
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope)[0].content.string_at(""), Some("SLASH-I1"));
        assert!(!compaction::invoked_skills::expired_message_ids_for_scope(scope).contains(&old_id));

        let backup = path.with_extension("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let fence = Arc::new(crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(token, Arc::new(tokio::sync::Mutex::new(()))));
        let id = ToolUseId::new();
        let accepted = if streaming {
            let stored = orch.append_streamed_query_row(message, Some(fence.clone())).await;
            orch.persist_preappended_stream_row(&stored, None, "2026-10-28T00:00:00.000Z", Some(fence), Some(id)).await
        } else {
            orch.append_committed_tool_skill(message, id, Some(fence)).await
        };
        assert!(!accepted, "a failed durable append must not activate the Skill");
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope)[0].content.string_at(""), Some("SLASH-I1"));
        assert!(!compaction::invoked_skills::expired_message_ids_for_scope(scope).contains(&old_id));
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let fence = Arc::new(crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(token, Arc::new(tokio::sync::Mutex::new(()))));
        let id = ToolUseId::new();
        let accepted = if streaming {
            let stored = orch.append_streamed_query_row(message, Some(fence.clone())).await;
            orch.persist_preappended_stream_row(&stored, None, "2026-10-28T00:00:00.000Z", Some(fence), Some(id)).await
        } else {
            orch.append_committed_tool_skill(message, id, Some(fence)).await
        };
        assert!(accepted);
        let active = compaction::invoked_skills::filter_for_scope(scope);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name.string_at(""), Some("review"));
        assert_eq!(active[0].content.string_at(""), Some("TOOL-Y"));
        let metadata = compaction::invoked_skills::activation_metadata_for_scope(scope);
        assert_ne!(metadata[0].definition_sha256, metadata[0].content_sha256);
        let definition_digest = metadata[0].definition_sha256;
        assert!(compaction::invoked_skills::expired_message_ids_for_scope(scope).contains(&old_id));
        orch.append_external_history_message(ConversationMessage::user(MessageId::new(), "task context ".repeat(300))).await;
        orch.append_external_history_message(ConversationMessage::Assistant {
            per_turn_effort: None, id: MessageId::new(), content: vec![ContentBlock::Text { text: "answer".into(), citations: None }], stop_reason: Some("end_turn".into()), resumable: None,
        }).await;
        let model = orch.session.lock().await.model.clone();
        orch.save_cache_safe_params(None, &model, &[]).await.unwrap();
        orch.maybe_compact_before_call(None).await;
        let model_text = orch.session.lock().await.model_context_history().iter().map(ConversationMessage::text_content).collect::<String>();
        assert!(model_text.contains("TOOL-Y"));
        assert!(!model_text.contains("TOOL-X"));
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(!source.contains("TOOL-X"));
        let loaded = session::jsonl::route_lines(&source);
        let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &sid.as_uuid().to_string());
        compaction::invoked_skills::reset_for_test();
        let cold = ConversationOrchestrator::new(
            OrchestratorConfig::default(), Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()), noop_hook_executor(), Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()), Arc::new(StaticMemoryProvider::empty()), root.path().to_path_buf(),
        ).with_session_id(sid);
        cold.restore_captured_session_history(sid.as_uuid(), &messages, &loaded.messages_in_order, false,
            permission::PermissionMode::Default, None, &source).await.unwrap();
        let cold_active = compaction::invoked_skills::filter_for_scope(scope);
        assert_eq!(cold_active.len(), 1);
        assert_eq!(cold_active[0].content.string_at(""), Some("TOOL-Y"));
        let cold_metadata = compaction::invoked_skills::activation_metadata_for_scope(scope);
        assert_ne!(cold_metadata[0].definition_sha256, cold_metadata[0].content_sha256);
        assert_eq!(cold_metadata[0].definition_sha256, definition_digest);
    }
    compaction::invoked_skills::reset_for_test();
}
