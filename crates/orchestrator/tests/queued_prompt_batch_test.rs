//! A queue batch keeps per-message origin metadata and runs one model turn.
use lingxi_core::host::FileSystem;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{
    scripted, ConversationOrchestrator, OrchestratorConfig, QueuedPromptInput, TurnOutcome,
};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn first_user_utf16_units(message: &ConversationMessage) -> Option<&[u16]> {
    let ConversationMessage::User { content, .. } = message else {
        return None;
    };
    match content.first()? {
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units),
        _ => None,
    }
}

#[tokio::test]
async fn queued_batch_preserves_each_meta_flag_uuid_and_jsonl_parent_in_one_turn() {
    for flags in [[false, true], [true, false], [true, true]] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let stream = || {
            scripted![
                message_start("msg_batch", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        };
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![stream(), stream()]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                api.clone(),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                orchestrator::test_support::noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
        );
        let ids = [MessageId::new(), MessageId::new()];
        let inputs: Vec<_> = flags
            .iter()
            .enumerate()
            .map(|(i, is_meta)| QueuedPromptInput {
                goal_retry_id: None,
                text: format!("queued text {i}").into(),
                is_meta: *is_meta,
                turn_companion: false,
                trailing_history_rows: Vec::new(),
                mod_origin: Some(if *is_meta {
                    serde_json::json!({"kind":"scheduled-trigger"})
                } else {
                    serde_json::json!({"kind":"bridge"})
                }),
                message_origin: (!*is_meta).then(|| {
                    lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                        r#"{"kind":"human"}"#,
                    )
                    .unwrap()
                }),
                prompt_source: None,
                message_id: Some(ids[i]),
                transcript_row_token: None,
                queue_priority: is_meta.then(|| "later".into()),
                scheduled_task_id: is_meta.then(|| format!("task-{i}")),
                scheduled_fire_id: Some(format!("fire-{i}")),
            })
            .collect();

        assert!(matches!(
            orch.run_queued_prompt_batch(vec![], CancellationToken::new())
                .await
                .unwrap(),
            TurnOutcome::EndTurn
        ));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            orch.run_queued_prompt_batch(inputs.clone(), cancelled)
                .await
                .unwrap(),
            TurnOutcome::Cancelled
        ));
        assert!(orch.snapshot_history().await.is_empty());
        assert!(api.captured_calls().await.is_empty());

        assert!(matches!(
            orch.run_queued_prompt_batch(inputs, CancellationToken::new())
                .await
                .unwrap(),
            TurnOutcome::EndTurn
        ));
        assert_eq!(
            api.captured_calls().await.len(),
            1,
            "one model request for the whole batch"
        );
        let history = orch.snapshot_history().await;
        let mut history_positions = Vec::new();
        for (i, expected) in flags.iter().enumerate() {
            let position = history
                .iter()
                .position(|message| message.id() == ids[i])
                .expect("each queued input keeps its caller-supplied identity");
            history_positions.push(position);
            assert!(
                matches!(&history[position], ConversationMessage::User { id, is_meta, .. } if *id == ids[i] && is_meta == expected)
            );
        }
        assert!(history_positions[0] < history_positions[1]);
        let records = JsonlReader::new(path.clone(), fs.clone())
            .read_all()
            .await
            .unwrap();
        let first_record_index = records
            .iter()
            .position(|record| record.uuid == ids[0].as_uuid().to_string())
            .expect("first queue row is persisted with its supplied identity");
        let second_record_index = records
            .iter()
            .position(|record| record.uuid == ids[1].as_uuid().to_string())
            .expect("second queue row is persisted with its supplied identity");
        assert!(first_record_index < second_record_index);
        assert_eq!(
            records[second_record_index].parent_uuid.as_deref(),
            Some(records[second_record_index - 1].uuid.as_str()),
            "intervening turn-ingress rows stay in the same JSONL parent chain"
        );
        for (i, is_meta) in flags.iter().enumerate() {
            assert_eq!(
                records[if i == 0 {
                    first_record_index
                } else {
                    second_record_index
                }]
                .extra
                .string_at("/queuePriority"),
                if *is_meta { Some("later") } else { None }
            );
        }
        assert!(
            !serde_json::to_string(&api.captured_calls().await[0].messages)
                .unwrap()
                .contains("queuePriority")
        );
        for (i, is_meta) in flags.iter().enumerate() {
            let record = &records[if i == 0 {
                first_record_index
            } else {
                second_record_index
            }];
            assert_eq!(
                record.extra.string_at("/scheduledTaskId"),
                is_meta.then(|| format!("task-{i}")).as_deref()
            );
            assert_eq!(
                record.extra.string_at("/scheduledFireId"),
                is_meta.then(|| format!("fire-{i}")).as_deref(),
                "fire id without task id stays absent"
            );
        }
        for (i, expected) in flags.iter().enumerate() {
            let record = &records[if i == 0 {
                first_record_index
            } else {
                second_record_index
            }];
            assert_eq!(
                record
                    .extra
                    .at_pointer("/isMeta")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                *expected
            );
            assert_eq!(
                record
                    .origin_projection
                    .as_ref()
                    .and_then(|origin| origin.string_at("/kind")),
                (!*expected).then_some("human"),
                "only a real queued prompt gets the explicit Native human origin"
            );
        }

        // No per-turn override can leak out of an all-meta or mixed batch.
        orchestrator::native_user_message_source::with_sdk_source(
            orch.run_turn_streaming_with_cancel("later sdk", CancellationToken::new()),
        )
        .await
        .unwrap();
        assert_eq!(api.captured_calls().await.len(), 2);
        let after = JsonlReader::new(path, fs).read_all().await.unwrap();
        assert!(after
            .iter()
            .rev()
            .find(|row| row.message_type == "user")
            .unwrap()
            .extra
            .at_pointer("/queuePriority")
            .is_none());
        let ordinary = after
            .iter()
            .rev()
            .find(|row| row.message_type == "user")
            .unwrap();
        assert!(!ordinary.extra.contains_property("scheduledTaskId"));
        assert!(!ordinary.extra.contains_property("scheduledFireId"));
        assert_eq!(ordinary.extra.string_at("/promptSource"), Some("sdk"));
        assert!(ordinary.origin_projection.is_none());
        let model_messages =
            serde_json::to_string(&api.captured_calls().await[0].messages).unwrap();
        assert!(
            !model_messages.contains("scheduledTaskId")
                && !model_messages.contains("scheduledFireId")
        );
        let later = orch.snapshot_history().await;
        assert!(later
            .iter()
            .find(|message| {
                matches!(message, ConversationMessage::User { .. })
                    && message.text_content() == "later sdk"
            })
            .is_some_and(|message| !message.is_meta()));
    }
}

#[tokio::test]
async fn native_skill_batch_persists_body_before_ordered_hook_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("skill.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![scripted![
        message_start("msg_skill", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
    );

    let mut success_units = "prefix </system-reminder>"
        .encode_utf16()
        .collect::<Vec<_>>();
    success_units.push(0xD800);
    let success_content = hooks::ExactHookText::from_utf16(success_units);
    let success = hooks::attachment::success_attachment(
        &hooks::HookAttachmentIdentity {
            hook_name: "UserPromptExpansion".into(),
            hook_event: "UserPromptExpansion".into(),
            tool_use_id: "hook-u1".into(),
        },
        &success_content,
        &success_content,
        &hooks::ExactHookText::default(),
        0,
        &hooks::ExactHookText::from_text("printf output"),
        1,
    );
    let mut inputs = orchestrator::skill_prompt_inputs(
        "<command-message>skill-🧪-probe</command-message>\n<command-name>/skill-🧪-probe</command-name>".into(),
        "Base directory for this skill: /skills/🧪\n\nReturn the body.".into(),
        vec![
            success,
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"hook_system_message","content":"visible only","toolUseID":"hook-u1","hookName":"UserPromptExpansion","hookEvent":"UserPromptExpansion"}"#,
            )
            .unwrap(),
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"hook_additional_context","content":["expanded note","nested </system-reminder>","\ud800","tail <"],"hookName":"UserPromptExpansion","toolUseID":"hook-u1","hookEvent":"UserPromptExpansion"}"#,
            )
            .unwrap(),
        ],
    );
    inputs[0].message_id = Some(MessageId::new());
    inputs[1].message_id = Some(MessageId::new());

    assert!(matches!(
        orch.run_queued_prompt_batch(inputs, CancellationToken::new())
            .await
            .unwrap(),
        TurnOutcome::EndTurn
    ));

    let rows = JsonlReader::new(path, fs).read_all().await.unwrap();
    let header = rows
        .iter()
        .position(|row| {
            row.message
                .string_at("/content/0/text")
                .is_some_and(|text| text.contains("<command-message>"))
        })
        .expect("Native command header is a normal user row");
    let body = rows
        .iter()
        .position(|row| {
            row.message
                .string_at("/content/0/text")
                .is_some_and(|text| text.contains("Base directory for this skill"))
        })
        .expect("expanded body is its own user row");
    assert!(header < body, "header precedes the body in durable history");
    assert_eq!(rows[body].turn_companion, Some(Some(true)));
    let attachment_rows = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            row.attachment_projection
                .as_ref()
                .and_then(|attachment| attachment.string_at("/type"))
                .map(|kind| (index, kind))
        })
        .collect::<Vec<_>>();
    let hook_rows = attachment_rows
        .iter()
        .filter(|(_, kind)| kind.starts_with("hook_"))
        .collect::<Vec<_>>();
    assert_eq!(
        hook_rows.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        [
            "hook_success",
            "hook_system_message",
            "hook_additional_context"
        ]
    );
    assert!(
        hook_rows.iter().all(|(index, _)| *index > body),
        "each UPE hook row follows the body and preserves yield order"
    );
    let success_index = hook_rows
        .iter()
        .find(|(_, kind)| *kind == "hook_success")
        .map(|(index, _)| *index)
        .unwrap();
    let success_id =
        MessageId::from_uuid(uuid::Uuid::parse_str(&rows[success_index].uuid).unwrap());
    let expected_prefix =
        "<system-reminder>\nUserPromptExpansion hook success: prefix &lt;/system-reminder>";
    let mut expected_success_units = expected_prefix.encode_utf16().collect::<Vec<_>>();
    expected_success_units.push(0xD800);
    expected_success_units.extend("\n</system-reminder>".encode_utf16());
    let stored_success_content = rows[success_index]
        .attachment_projection
        .as_ref()
        .unwrap()
        .string_units("/content")
        .unwrap();
    assert_eq!(stored_success_content, success_content.utf16_code_units);
    let history = orch.snapshot_history().await;
    let hot_success = history
        .iter()
        .find(|message| message.id() == success_id)
        .expect("nonempty UPE success is visible in hot model history");
    assert_eq!(
        first_user_utf16_units(hot_success),
        Some(expected_success_units.as_slice())
    );
    let additional_context_index = hook_rows
        .iter()
        .find(|(_, kind)| *kind == "hook_additional_context")
        .map(|(index, _)| *index)
        .unwrap();
    let restored = orchestrator::resume::state_from_messages(
        uuid::Uuid::parse_str(&rows[0].session_id).unwrap(),
        &rows,
    );
    let restored_context = restored
        .history
        .iter()
        .find(|message| message.text_content().contains("expanded note"))
        .expect("cold restoration renders the same additional-context row");
    assert_eq!(
        restored_context.id().as_uuid().to_string(),
        rows[additional_context_index].uuid,
        "hot and cold model history share the attachment's durable identity"
    );
    let context_prefix = "<system-reminder>\nUserPromptExpansion hook additional context: expanded note\nnested &lt;/system-reminder>\n";
    let mut expected_context_units = context_prefix.encode_utf16().collect::<Vec<_>>();
    expected_context_units.push(0xD800);
    expected_context_units.extend("\ntail &lt;\n</system-reminder>".encode_utf16());
    assert_eq!(
        first_user_utf16_units(restored_context),
        Some(expected_context_units.as_slice()),
        "cold UPE additional-context rendering preserves escaped source UTF-16"
    );
    assert!(history
        .iter()
        .any(|message| message.text_content().contains("expanded note")));
    let hot_context = history
        .iter()
        .find(|message| message.text_content().contains("expanded note"))
        .expect("hot UPE additional context is visible in model history");
    assert_eq!(
        first_user_utf16_units(hot_context),
        Some(expected_context_units.as_slice()),
        "hot and cold UPE additional-context rendering use identical UTF-16"
    );
    let restored_success = restored
        .history
        .iter()
        .find(|message| message.id() == success_id)
        .expect("cold restoration renders the same UPE success row");
    assert_eq!(
        first_user_utf16_units(restored_success),
        Some(expected_success_units.as_slice()),
        "hot and cold rendering preserve the Native UTF-16 payload"
    );
    let requests = api.captured_calls().await;
    assert_eq!(requests.len(), 1);
    let captured_messages = &requests[0].messages;
    let header_request_index = captured_messages
        .iter()
        .position(|message| {
            message
                .text_content()
                .starts_with("<command-message>skill-🧪-probe")
        })
        .expect("the orchestrator mock observes a distinct pre-SDK command header row");
    let body_request_index = captured_messages
        .iter()
        .position(|message| {
            message
                .text_content()
                .contains("Base directory for this skill")
        })
        .expect("the orchestrator mock observes a distinct pre-SDK body row");
    let success_request_index = captured_messages
        .iter()
        .position(|message| message.id() == success_id)
        .expect("the orchestrator mock observes a distinct pre-SDK hook-success row");
    let context_request_index = captured_messages
        .iter()
        .position(|message| {
            message.id().as_uuid().to_string() == rows[additional_context_index].uuid
        })
        .expect("the orchestrator mock observes a distinct pre-SDK additional-context row");
    assert!(
        header_request_index < body_request_index
            && body_request_index < success_request_index
            && success_request_index < context_request_index,
        "pre-SDK source rows retain header/body/UPE-yield order"
    );
    assert!(matches!(
        &captured_messages[header_request_index],
        ConversationMessage::User { is_meta: false, .. }
    ));
    assert!(matches!(
        &captured_messages[body_request_index],
        ConversationMessage::User {
            is_meta: true,
            turn_companion: Some(Some(true)),
            ..
        }
    ));
    let batch_user_rows = captured_messages[header_request_index..=context_request_index].to_vec();
    assert!(batch_user_rows
        .iter()
        .all(|message| matches!(message, ConversationMessage::User { .. })));
    let sdk_normalized = llm_runtime::convert::normalize_messages_for_api(batch_user_rows);
    assert_eq!(
        sdk_normalized.len(),
        1,
        "the production SDK normalizer merges the captured adjacent user batch"
    );
    let outgoing_text = sdk_normalized[0].text_content();
    assert!(
        outgoing_text.starts_with(
            "<command-message>skill-🧪-probe</command-message>\n<command-name>/skill-🧪-probe</command-name>\nBase directory for this skill: /skills/🧪\n\nReturn the body."
        ),
        "the Native message seam inserts a newline between the command header and body: {outgoing_text:?}"
    );
    assert!(outgoing_text.contains(
        "<system-reminder>\nUserPromptExpansion hook additional context: expanded note\nnested &lt;/system-reminder>\n\u{FFFD}\ntail &lt;\n</system-reminder>"
    ));
    assert!(outgoing_text.contains(
        "<system-reminder>\nUserPromptExpansion hook success: prefix &lt;/system-reminder>\u{FFFD}\n</system-reminder>"
    ));
    assert!(!history
        .iter()
        .any(|message| message.text_content().contains("visible only")));
    let request = serde_json::to_string(&api.captured_calls().await[0].messages).unwrap();
    assert!(request.contains("expanded note"));
    assert!(!request.contains("visible only"));
}

#[tokio::test]
async fn native_skill_batch_batched_driver_keeps_source_and_companion_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("skill-batched.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            api.clone(),
            Arc::new(MockStreamingApiClient::with_turns(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
    );
    let success_content = hooks::ExactHookText::from_text("batched plain stdout");
    let success = hooks::attachment::success_attachment(
        &hooks::HookAttachmentIdentity {
            hook_name: "UserPromptExpansion".into(),
            hook_event: "UserPromptExpansion".into(),
            tool_use_id: "hook-u2".into(),
        },
        &success_content,
        &success_content,
        &hooks::ExactHookText::default(),
        0,
        &hooks::ExactHookText::from_text("printf output"),
        1,
    );
    let inputs = orchestrator::skill_prompt_inputs(
        "<command-message>skill</command-message>\n<command-name>/skill</command-name>".into(),
        "Base directory for this skill: /skills/skill\n\nbody".into(),
        vec![
            success,
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"hook_additional_context","content":["context"],"hookName":"UserPromptExpansion","toolUseID":"hook-u2","hookEvent":"UserPromptExpansion"}"#,
            )
            .unwrap(),
        ],
    );
    assert!(matches!(
        orchestrator::native_user_message_source::with_sdk_source(
            orch.run_queued_prompt_batch_batched(inputs, CancellationToken::new())
        )
        .await
        .unwrap(),
        TurnOutcome::EndTurn
    ));

    let rows = JsonlReader::new(path, fs).read_all().await.unwrap();
    let users = rows
        .iter()
        .filter(|row| row.message_type == "user")
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].extra.string_at("/promptSource"), Some("sdk"));
    assert!(users[0].origin_projection.is_none());
    assert_eq!(users[1].turn_companion, Some(Some(true)));
    assert!(users[1].origin_projection.is_none());
    assert!(rows.iter().any(|row| {
        row.attachment_projection
            .as_ref()
            .and_then(|attachment| attachment.string_at("/type"))
            == Some("hook_additional_context")
    }));
    assert!(rows.iter().any(|row| {
        row.attachment_projection
            .as_ref()
            .and_then(|attachment| attachment.string_at("/type"))
            == Some("hook_success")
    }));
    let request = api.captured_msgs().await;
    assert_eq!(request.len(), 1);
    assert!(request[0]
        .iter()
        .any(|message| message.text_content().contains("context")));
    assert!(request[0].iter().any(|message| {
        message
            .text_content()
            .contains("UserPromptExpansion hook success: batched plain stdout")
    }));
}

#[tokio::test]
async fn skill_upe_history_precedes_mod_context_in_both_batch_drivers() {
    for streaming in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skill-hook-order.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let module = dir.path().join("prompt-submit.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.submit', ($, e, next) => {
                if (e.text.startsWith('<command-message>')) {
                  return next({ ...e, context: ['Mod context'] });
                }
                return next(e);
              });
            }
        "#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "skill-order",
            dir.path(),
            &module,
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
        )
        .await
        .unwrap();
        let mut hook_registry = hooks::HookRegistry::new();
        hook_registry.set_mod_host(host);

        let api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let stream = || {
            scripted![
                message_start("msg_skill_order", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        };
        let streaming_api = Arc::new(MockStreamingApiClient::with_turns(vec![stream()]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                api.clone(),
                streaming_api.clone(),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                orchestrator::test_support::noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)))
            .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
        );

        let mut inputs = orchestrator::skill_prompt_inputs(
            "<command-message>skill</command-message>\n<command-name>/skill</command-name>".into(),
            "Base directory for this skill: /skills/skill\n\nbody".into(),
            vec![
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    r#"{"type":"hook_system_message","content":"system","toolUseID":"hook-skill","hookName":"UserPromptExpansion","hookEvent":"UserPromptExpansion"}"#,
                )
                .unwrap(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    r#"{"type":"hook_additional_context","content":["UPE context"],"toolUseID":"hook-skill","hookName":"UserPromptExpansion","hookEvent":"UserPromptExpansion"}"#,
                )
                .unwrap(),
            ],
        );
        inputs[0].mod_origin = Some(serde_json::json!({"kind":"composer"}));

        let outcome = if streaming {
            orch.run_queued_prompt_batch(inputs, CancellationToken::new())
                .await
                .unwrap()
        } else {
            orch.run_queued_prompt_batch_batched(inputs, CancellationToken::new())
                .await
                .unwrap()
        };
        assert!(matches!(outcome, TurnOutcome::EndTurn));

        let records = JsonlReader::new(path, fs).read_all().await.unwrap();
        let body_index = records
            .iter()
            .position(|record| {
                record
                    .message
                    .string_at("/content/0/text")
                    .is_some_and(|text| text.contains("Base directory for this skill"))
            })
            .expect("expanded body row is persisted");
        let hook_events = records
            .iter()
            .filter_map(|record| {
                let attachment = record.attachment_projection.as_ref()?;
                let event = attachment.string_at("/hookEvent")?;
                matches!(event, "UserPromptExpansion" | "UserPromptSubmit").then_some(event)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            hook_events,
            [
                "UserPromptExpansion",
                "UserPromptExpansion",
                "UserPromptSubmit"
            ],
            "the UPE rows from Qe remain contiguous immediately after the body and precede host Mod contexts; streaming={streaming}"
        );
        let first_upe_index = records
            .iter()
            .position(|record| {
                record
                    .attachment_projection
                    .as_ref()
                    .is_some_and(|row| row.string_at("/hookEvent") == Some("UserPromptExpansion"))
            })
            .unwrap();
        assert_eq!(first_upe_index, body_index + 1);

        let outgoing = if streaming {
            streaming_api.captured_calls().await[0].messages.clone()
        } else {
            api.captured_msgs().await[0].clone()
        };
        let upe_context = outgoing
            .iter()
            .position(|message| {
                message
                    .text_content()
                    .contains("UserPromptExpansion hook additional context: UPE context")
            })
            .expect("UPE context reaches the model");
        let mod_context = outgoing
            .iter()
            .position(|message| {
                message
                    .text_content()
                    .contains("prompt.submit hook additional context: Mod context")
            })
            .expect("Mod context reaches the model");
        assert!(upe_context < mod_context, "streaming={streaming}");
    }
}

#[tokio::test]
async fn prompt_submit_mod_screens_each_batch_entry_before_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let module = dir.path().join("submit.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.submit', ($, e, next) => {
            if (e.origin.kind === 'plugin') return { drop: 'plugin refused' };
            if (e.origin.kind === 'bridge') {
              return next({ ...e, text: 'rewritten bridge', context: ['batch note'] });
            }
            return next(e);
          });
        }
    "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "submit",
        dir.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&serde_json::json!(
            {}
        ))
        .expect("generated Mod options are representable"),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![scripted![
        message_start("msg_submit", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
        .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
    );
    let inputs = vec![
        QueuedPromptInput {
            text: "typed bridge".into(),
            mod_origin: Some(serde_json::json!({"kind":"bridge"})),
            ..Default::default()
        },
        QueuedPromptInput {
            text: "timer".into(),
            is_meta: true,
            mod_origin: Some(serde_json::json!({"kind":"scheduled-trigger"})),
            ..Default::default()
        },
        QueuedPromptInput {
            text: "plugin text".into(),
            is_meta: true,
            mod_origin: Some(serde_json::json!({"kind":"plugin","name":"source"})),
            ..Default::default()
        },
    ];
    orch.run_queued_prompt_batch(inputs, CancellationToken::new())
        .await
        .unwrap();
    let calls = api.captured_calls().await;
    assert_eq!(calls.len(), 1);
    let model_text: Vec<_> = calls[0]
        .messages
        .iter()
        .map(ConversationMessage::text_content)
        .collect();
    let rewritten = model_text
        .iter()
        .position(|text| text == "rewritten bridge")
        .unwrap();
    assert!(model_text[rewritten + 1].contains("prompt.submit hook additional context: batch note"));
    assert_eq!(model_text[rewritten + 2], "timer");
    assert!(!model_text
        .iter()
        .any(|text| text.contains("plugin text") || text.contains("typed bridge")));
    let records = JsonlReader::new(path, fs).read_all().await.unwrap();
    let bridge_record_index = records
        .iter()
        .position(|record| {
            record.message_type == "user"
                && record.message.string_at("/content/0/text") == Some("rewritten bridge")
        })
        .expect("the persisted bridge prompt uses the prompt.submit rewrite");
    let hook_record_index = records
        .iter()
        .position(|record| {
            record.message_type == "attachment"
                && record
                    .attachment_projection
                    .as_ref()
                    .and_then(|payload| payload.string_at("/hookName"))
                    == Some("prompt.submit")
        })
        .expect("the prompt.submit context is persisted as its own attachment");
    let timer_record_index = records
        .iter()
        .position(|record| {
            record.message_type == "user"
                && record.message.string_at("/content/0/text") == Some("timer")
        })
        .expect("the scheduled timer row is persisted");
    assert!(bridge_record_index < hook_record_index && hook_record_index < timer_record_index);
    assert!(serde_json::to_string(&records[bridge_record_index])
        .unwrap()
        .contains("rewritten bridge"));
    assert!(serde_json::to_string(&records[timer_record_index])
        .unwrap()
        .contains("timer"));
    assert!(!serde_json::to_string(&records)
        .unwrap()
        .contains("plugin text"));
}
