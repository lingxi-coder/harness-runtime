use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
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

async fn logged_inputs(output: &MockOutputStream) -> Vec<serde_json::Value> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text }
                if plugin == "session-append" =>
            {
                text.strip_prefix("append:")
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
    host.load("session-append", root, &module, serde_json::json!({}))
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
        id: MessageId::new(),
        content: vec![
            ContentBlock::Text {
                text: "merged-first".into(),
                citations: None,
            },
            ContentBlock::ToolUse {
                id: tool_use_id.clone(),
                name: "Read".into(),
                input: serde_json::json!({"file_path":"source.txt"}),
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
        input: serde_json::json!({"file_path":"source.txt"}),
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
    let disk_content = rows[0].message["content"]
        .as_array()
        .expect("assistant content array");
    assert_eq!(disk_content[0]["text"], "merged-edited");
    assert_eq!(disk_content[1]["text"], "merged-added");
    assert_eq!(disk_content[2], serde_json::to_value(image).unwrap());
    assert_eq!(disk_content[3], serde_json::to_value(document).unwrap());
    assert_eq!(disk_content[4]["id"], tool_use_id.to_string());
    assert_eq!(disk_content[4]["provider_id"], "toolu_source");
    assert_eq!(disk_content[4]["name"], "Read");
    assert_eq!(
        disk_content[4]["input"],
        serde_json::json!({"file_path":"source.txt"})
    );
    assert!(!disk_content
        .iter()
        .any(|block| block["text"] == "remove this text"));

    let result = ConversationMessage::User {
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
    let persisted_result = &rows[1].message["content"][0];
    assert_eq!(persisted_result["type"], "tool_result");
    assert_eq!(persisted_result["tool_use_id"], tool_use_id.to_string());
    assert_eq!(persisted_result["provider_tool_use_id"], "toolu_source");
    assert_eq!(persisted_result["content"], "rewritten tool output");
    assert_eq!(persisted_result["is_error"], true);
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
    assert_eq!(content, "rewritten tool output");
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
    assert_eq!(rows[2].message["content"][0]["text"], "throw-original");
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
        id: MessageId::new(),
        content: vec![
            ContentBlock::Text {
                text: "streamed-text".into(),
                citations: None,
            },
            ContentBlock::ToolUse {
                id: tool_use_id.clone(),
                name: "Bash".into(),
                input: serde_json::json!({"command":"pwd"}),
                provider_id: Some("toolu_streamed".into()),
            },
        ],
        stop_reason: Some("tool_use".into()),
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
            input: serde_json::json!({"command":"pwd"}),
            provider_id: Some("toolu_streamed".into()),
        }
    );

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 2);
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["uuid"], rows[0].uuid);
    assert_eq!(inputs[1]["uuid"], rows[1].uuid);
    assert_eq!(rows[0].message["content"][0]["text"], "streamed-edited");
    assert_eq!(rows[1].message["content"][0]["id"], tool_use_id.to_string());
    assert_eq!(
        rows[1].message["content"][0]["provider_id"],
        "toolu_streamed"
    );
    assert_eq!(rows[1].message["content"][0]["name"], "Bash");
    assert_eq!(
        rows[1].message["content"][0]["input"],
        serde_json::json!({"command":"pwd"})
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
        id: MessageId::new(),
        content: vec![
            ContentBlock::ToolUse {
                id: duplicate_id.clone(),
                name: "Bash".into(),
                input: serde_json::json!({"command":"first"}),
                provider_id: None,
            },
            ContentBlock::ToolUse {
                id: duplicate_id.clone(),
                name: "Read".into(),
                input: serde_json::json!({"file":"second"}),
                provider_id: None,
            },
        ],
        stop_reason: Some("tool_use".into()),
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
            input: serde_json::json!({"command":"first"}),
            provider_id: None,
        },
        ContentBlock::Text {
            text: "suffix".into(),
            citations: Some(None),
        },
        ContentBlock::ToolUse {
            id: duplicate_id,
            name: "Read".into(),
            input: serde_json::json!({"file":"second"}),
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
    let stored = rows[0].message["content"]
        .as_array()
        .expect("persisted content array");
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[0]["text"], "prefix");
    assert_eq!(stored[1]["name"], "Bash");
    assert_eq!(stored[2]["text"], "suffix");
    assert_eq!(stored[3]["name"], "Read");
    assert_eq!(stored[4]["text"], "suffix");
    assert_eq!(logged_inputs(&output).await.len(), 1);
}

#[tokio::test]
async fn compact_boundary_session_append_keeps_metadata_and_rewrites_disk_and_history_together() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("session.jsonl");
    let (orchestrator, output) = build_orchestrator(root.path(), &path, REWRITE_MOD).await;
    let marker = ConversationMessage::System {
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
            serde_json::json!({"trigger":"manual","preTokens":128}),
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
    assert_eq!(content, "rewritten compact boundary");
    assert_eq!(subtype.as_deref(), Some("compact_boundary"));
    assert_eq!(compact_metadata.as_ref().unwrap().pre_tokens, 128);

    let rows = read_rows(&path);
    assert_eq!(rows.len(), 1);
    let inputs = logged_inputs(&output).await;
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["message"]["name"], "compact_boundary");
    assert_eq!(inputs[0]["door"], "compaction");
    assert_eq!(inputs[0]["uuid"], rows[0].uuid);
    assert_eq!(rows[0].extra["content"], "rewritten compact boundary");
    assert_eq!(rows[0].extra["subtype"], "compact_boundary");
    assert_eq!(rows[0].extra["compactMetadata"]["trigger"], "manual");
    assert_eq!(rows[0].extra["compactMetadata"]["preTokens"], 128);
}
