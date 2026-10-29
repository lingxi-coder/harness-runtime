include!("../fast_mode_preference_tests.rs");
include!("../permission_preference_tests.rs");
include!("../model_preference_tests.rs");
include!("../provider_region_tests.rs");
include!("../reasoning_preference_tests.rs");
#[path = "bash_precommit_wiring_tests.rs"]
mod bash_precommit_wiring_tests;
#[path = "device_tools_tests.rs"]
mod device_tools_tests;
#[path = "prompt_suggestion_tests.rs"]
mod prompt_suggestion_tests;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::mobile::turn_durability::QueueDeliveryState;
use async_trait::async_trait;
use client::adapter::{ClientEventListener, ListenerSink, PermissionRequestSink};
use client::protocol::events::ClientEvent;
use client::protocol::listings::SessionModeDto;
use client::protocol::text::ClientTextDto;
use lingxi_core::host::subagent_spawn::{SubagentObservation, SubagentSpawnObserver};
use lingxi_core::host::{
    OrchestratorHandle as _, SlashCommandDispatcher as _, SlashDispatchResult,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tokio::sync::{Notify, RwLock};
use tool_skill::skill::SkillCommandType;

use super::{
    build_mobile, builtin_provider_catalog, classify_provider_connection_response,
    collect_session_agent_transcript_paths, enqueue_mobile_prompt_dispatch,
    find_session_agent_transcript_path, lower_session_agent_snapshot, mobile_cron_schedule_error,
    mobile_mcp_config_unchanged, mobile_mcp_oauth_authorization_callback, mobile_mcp_preflight,
    mobile_mcp_record_reload_intent, mobile_mcp_reload_requires_replacement,
    mobile_mcp_run_reload_job, mobile_mcp_state_is_transitional,
    session_agent_conversation_is_visible, session_agent_transcript_event,
    session_agent_transcript_revision, McpRegistry, McpServerConfig, MobileConfig,
    MobileCronStoreHandle, MobileMcpReloadJob, MobileMsgQueueInput, MobileSessionAgentObserver,
};

fn source_text(text: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::from(text)
}

#[tokio::test]
async fn mobile_midturn_source_acks_last_identity_and_ordered_members() {
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;

    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    let first = store
        .admit_queued_prompt(
            "session",
            None,
            Some(71),
            source_text("first"),
            None,
            vec![client::protocol::commands::ImageRefDto {
                media_type: "image/png".into(),
                base64: "Zmlyc3Q=".into(),
            }],
        )
        .expect("first durable member");
    let last = store
        .admit_queued_prompt(
            "session",
            None,
            Some(72),
            source_text("last"),
            None,
            vec![client::protocol::commands::ImageRefDto {
                media_type: "image/jpeg".into(),
                base64: "bGFzdA==".into(),
            }],
        )
        .expect("last durable member");
    let first_identity = first.prompt_identity.clone().unwrap();
    let last_identity = last.prompt_identity.clone().unwrap();
    assert_ne!(first_identity.message_id, last_identity.message_id);

    let queue = Arc::new(msgqueue::MessageQueueManager::new());
    enqueue_mobile_prompt_dispatch(&queue, "session", &first).await;
    enqueue_mobile_prompt_dispatch(&queue, "session", &last).await;
    let source = MobileMsgQueueInput {
        queue: queue.clone(),
        loop_state: Arc::new(tool_cron::LoopRuntime::default()),
        durable_turns: store.clone(),
        active_session_uuid: Arc::new(std::sync::Mutex::new("session".into())),
    };

    let inputs = source
        .take_mid_turn_batch()
        .await
        .unwrap()
        .expect("admit both mobile source members");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].text.string_at(""), Some("first"));
    assert_eq!(inputs[1].text.string_at(""), Some("last"));
    assert!(matches!(
        &inputs[0].images[0],
        lingxi_core::types::ImageSource::Base64 { media_type, data }
            if media_type == "image/png" && data == "Zmlyc3Q="
    ));
    assert!(matches!(
        &inputs[1].images[0],
        lingxi_core::types::ImageSource::Base64 { media_type, data }
            if media_type == "image/jpeg" && data == "bGFzdA=="
    ));
    let identity = inputs[0].pending_prompt_identity.as_ref().unwrap();
    assert_eq!(identity.owner_turn_id, first.turn_id);
    assert_eq!(
        identity.transcript_row_token,
        Some("client-turn:71,72".into())
    );

    assert_eq!(identity.message_id, last_identity.message_id);
    assert_eq!(identity.prompt_id, last_identity.prompt_id);
    assert_eq!(identity.member_turn_ids, [first.turn_id, last.turn_id]);

    source.input_consumed(&inputs).await.unwrap();
    let owner = store
        .load("session", first.turn_id)
        .expect("read acknowledged owner envelope");
    assert_eq!(
        owner.queue_delivery,
        crate::mobile::turn_durability::QueueDeliveryState::Delivered
    );
    let persisted = owner.coalesced_batch.unwrap();
    assert_eq!(persisted.owner_turn_id, first.turn_id);
    assert_eq!(persisted.aggregate_identity, last_identity);
    assert_eq!(
        persisted
            .member_identities
            .iter()
            .map(|member| member.prompt_identity.clone())
            .collect::<Vec<_>>(),
        [first_identity, last_identity]
    );
    assert!(queue.snapshot().await.is_empty());
}

#[tokio::test]
async fn mobile_midturn_batches_stop_at_bash_and_never_join_commands() {
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;

    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    let normal_a = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("normal-a"),
            Some(client::protocol::commands::PromptModeDto::Normal),
            Vec::new(),
        )
        .expect("first normal prompt");
    let normal_b = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("normal-b"),
            None,
            Vec::new(),
        )
        .expect("second normal prompt");
    let bash_a = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("echo a"),
            Some(client::protocol::commands::PromptModeDto::Bash),
            Vec::new(),
        )
        .expect("first Bash command");
    let bash_b = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("echo b"),
            Some(client::protocol::commands::PromptModeDto::Bash),
            Vec::new(),
        )
        .expect("second Bash command");
    let normal_c = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("normal-c"),
            Some(client::protocol::commands::PromptModeDto::Normal),
            Vec::new(),
        )
        .expect("normal prompt after Bash commands");

    let queue = Arc::new(msgqueue::MessageQueueManager::new());
    for checkpoint in [&normal_a, &normal_b, &bash_a, &bash_b, &normal_c] {
        enqueue_mobile_prompt_dispatch(&queue, "session", checkpoint).await;
    }
    let source = MobileMsgQueueInput {
        queue: queue.clone(),
        loop_state: Arc::new(tool_cron::LoopRuntime::default()),
        durable_turns: store.clone(),
        active_session_uuid: Arc::new(std::sync::Mutex::new("session".into())),
    };

    let normal_batch = source
        .take_mid_turn_batch()
        .await
        .unwrap()
        .expect("contiguous normal prefix");
    assert_eq!(
        normal_batch
            .iter()
            .map(|input| input.text.string_at("").unwrap())
            .collect::<Vec<_>>(),
        ["normal-a", "normal-b"]
    );
    assert_eq!(
        normal_batch[0]
            .pending_prompt_identity
            .as_ref()
            .unwrap()
            .member_turn_ids,
        [normal_a.turn_id, normal_b.turn_id]
    );
    source.input_consumed(&normal_batch).await.unwrap();

    assert!(
        source.take_mid_turn_batch().await.unwrap().is_none(),
        "the mid-turn normal-input path must leave Bash for its direct owner"
    );
    assert_eq!(
        queue
            .snapshot()
            .await
            .iter()
            .map(|command| command.uuid.clone())
            .collect::<Vec<_>>(),
        [
            format!("mobile-prompt:session:{}", bash_a.turn_id),
            format!("mobile-prompt:session:{}", bash_b.turn_id),
            format!("mobile-prompt:session:{}", normal_c.turn_id),
        ],
        "Bash and later work remain queued in source order"
    );
    let pending = store.scan_pending("session").expect("scan remaining work");
    assert_eq!(
        pending
            .iter()
            .map(|entry| entry.turn_id)
            .collect::<Vec<_>>(),
        [bash_a.turn_id, bash_b.turn_id, normal_c.turn_id]
    );
    assert!(pending.iter().all(|entry| entry.coalesced_batch.is_none()));
    assert!(matches!(
        store.prepare_mid_turn_batch("session", &[bash_a.turn_id, bash_b.turn_id]),
        Err(crate::mobile::turn_durability::DurableTurnStoreError::Storage(message))
            if message.contains("Bash execution boundary")
    ));
}

fn source_value(value: &serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("test JSON value is valid source JSON")
}

fn source_string_contains(value: &Utf16JsonProjection, needle: &str) -> bool {
    let Some(units) = value.string_units("") else {
        return false;
    };
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    if needle.is_empty() {
        return true;
    }
    units
        .windows(needle.len())
        .any(|candidate| candidate == needle.as_slice())
}

fn source_string_starts_with(value: &Utf16JsonProjection, prefix: &str) -> bool {
    let Some(units) = value.string_units("") else {
        return false;
    };
    let prefix = prefix.encode_utf16().collect::<Vec<_>>();
    units.starts_with(&prefix)
}

#[tokio::test]
async fn cancel_before_midturn_admission_keeps_uncancelled_mobile_dispatch_members() {
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;

    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    let first = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("first"),
            None,
            Vec::new(),
        )
        .expect("first durable member");
    let second = store
        .admit_queued_prompt(
            "session",
            None,
            None,
            source_text("second"),
            None,
            Vec::new(),
        )
        .expect("second durable member");
    let queue = Arc::new(msgqueue::MessageQueueManager::new());
    enqueue_mobile_prompt_dispatch(&queue, "session", &first).await;
    enqueue_mobile_prompt_dispatch(&queue, "session", &second).await;
    assert_eq!(
        store
            .cancel_pending("session", second.turn_id)
            .expect("cancel only the selected unclaimed member"),
        Some(vec![second.turn_id])
    );

    let source = MobileMsgQueueInput {
        queue: queue.clone(),
        loop_state: Arc::new(tool_cron::LoopRuntime::default()),
        durable_turns: store,
        active_session_uuid: Arc::new(std::sync::Mutex::new("session".into())),
    };
    assert!(source.take_mid_turn_batch().await.unwrap().is_none());
    assert_eq!(
        queue
            .snapshot()
            .await
            .iter()
            .map(|command| command.uuid.clone())
            .collect::<Vec<_>>(),
        [format!("mobile-prompt:session:{}", first.turn_id)]
    );

    let surviving = source
        .take_mid_turn_batch()
        .await
        .unwrap()
        .expect("the uncancelled checkpoint is still dispatchable");
    assert_eq!(surviving.len(), 1);
    assert_eq!(surviving[0].text.string_at(""), Some("first"));
    let identity = surviving[0].pending_prompt_identity.as_ref().unwrap();
    assert_eq!(identity.owner_turn_id, first.turn_id);
    assert_eq!(identity.member_turn_ids, [first.turn_id]);
}

fn client_text_contains(value: &ClientTextDto, needle: &str) -> bool {
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    if needle.is_empty() {
        return true;
    }
    value
        .code_units
        .windows(needle.len())
        .any(|candidate| candidate == needle.as_slice())
}

fn replayed_session_from_jsonl(raw: &str) -> orchestrator::ReplayedSession {
    let messages = session::jsonl::reader::route_lines(raw).messages_in_order;
    let state = orchestrator::state_from_messages(uuid::Uuid::new_v4(), &messages);
    let runtime_metadata = orchestrator::runtime_metadata_from_messages(&messages);
    orchestrator::ReplayedSession {
        transcript_path: std::path::PathBuf::new(),
        working_directory: None,
        worktree_session: None,
        transcript_metadata: Vec::new(),
        display_history: state.history.clone(),
        display_timestamps: Default::default(),
        state,
        messages,
        last_message_uuid: None,
        runtime_metadata,
        client_state_tool_results: Default::default(),
    }
}
use mcp::ConfigScope as McpConfigScope;

#[test]
fn mobile_provider_catalog_matches_engine_presets_without_secrets() {
    let dto = builtin_provider_catalog();
    let catalog = llm_runtime::builtin_presets();

    let anthropic = dto
        .iter()
        .find(|entry| entry.profile_id == "anthropic")
        .expect("catalog must include the first-party Anthropic profile");
    assert_eq!(anthropic.display_name, "Anthropic");
    assert_eq!(anthropic.base_url, "https://api.anthropic.com");
    assert_eq!(anthropic.protocol, "AnthropicMessages");
    assert_eq!(
        anthropic.credential_env.as_deref(),
        Some("ANTHROPIC_API_KEY")
    );

    assert_eq!(
        dto.len(),
        catalog
            .providers
            .iter()
            .filter(|provider| !provider.connection.hidden)
            .count()
    );
    assert_eq!(
        dto.iter()
            .filter(|entry| entry.profile_id == "anthropic")
            .count(),
        1
    );
    for (entry, provider) in
        dto.iter()
            .filter(|entry| entry.profile_id != "anthropic")
            .zip(catalog.providers.iter().filter(|provider| {
                provider.profile_name != "anthropic" && !provider.connection.hidden
            }))
    {
        assert_eq!(entry.profile_id, provider.profile_name);
        assert_eq!(
            entry.display_name,
            provider
                .wire_profile
                .as_ref()
                .and_then(|profile| profile.info.display_name.clone())
                .unwrap_or_else(|| provider.profile_name.clone())
        );
        assert_eq!(entry.base_url, provider.base_url);
        assert_eq!(entry.protocol, format!("{:?}", provider.protocol));
        assert_eq!(entry.auth, format!("{:?}", provider.auth));
        assert_eq!(
            entry.credential_env,
            match &provider.credential {
                llm_runtime::CredentialConfig::Env { var } => Some(var.clone()),
                _ => None,
            }
        );
        assert_eq!(
            entry.models,
            provider
                .models
                .iter()
                .filter(|model| {
                    lingxi_core::host::is_curated_model(
                        &provider.profile_name,
                        &model.request_model,
                    ) || !lingxi_core::host::provider_has_curated_list(&provider.profile_name)
                })
                .map(|model| model.request_model.clone())
                .collect::<Vec<_>>()
        );
        assert!(entry
            .credential_env
            .as_deref()
            .is_none_or(|env| !env.contains("KEY=")));
    }
}
// F3-06: the off-device host shim now lives in `crate::mobile::test_support` (the
// single, non-drifting definition shared with the `skeleton_test.rs`
// integration test). The in-crate F3-03/F3-05 unit tests reuse it. The
// collecting permission sink is aliased to the legacy name these test bodies
// already use.
use crate::mobile::test_support::{
    test_config, CollectingPermissionSink as RecordingPermissionSink, FakeListener,
    HostFakePlatform,
};

/// Composition regression for the mobile registration path: the Workflow
/// instance retained by `build_mobile` must use the enforcing policy gate
/// that the runtime built. A directory at `scriptPath` makes any
/// pre-authorization read fail, while the settings deny proves the live
/// Read policy is consulted first.
#[tokio::test]
async fn mobile_workflow_script_path_is_read_gated_before_launcher_io() {
    use permission::PermissionResult;
    use tool_api::Tool as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    let settings_dir = tmp.path().join(branding::DOT_DIR);
    std::fs::create_dir_all(&settings_dir).expect("settings directory");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"permissions":{"deny":["Read(./secret.js)"]}}"#,
    )
    .expect("permission settings");
    let script_path = tmp.path().join("secret.js");
    std::fs::create_dir(&script_path).expect("directory path must be unreadable as a script");

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let permission_sink: Arc<dyn PermissionRequestSink> =
        Arc::new(RecordingPermissionSink::default());
    let runtime = build_mobile(test_config(tmp.path()), platform, listener, permission_sink)
        .await
        .expect("mobile build must succeed without reading scriptPath");
    let input = source_value(&serde_json::json!({
        "scriptPath": script_path.to_string_lossy().into_owned()
    }));
    let ctx = tool_api::test_support::fresh_ctx();

    runtime
        .wired_workflow_tool
        .validate_input(&input, &ctx)
        .await
        .expect("scriptPath shape validation must not touch the directory");
    let decision = runtime
        .wired_workflow_tool
        .check_permissions(&input, &ctx)
        .await;
    assert!(matches!(decision, PermissionResult::Deny { .. }));
    let rendered = format!("{decision:?}");
    assert!(!rendered.contains("secret workflow contents"));
}

#[tokio::test]
async fn session_agent_helpers_find_nested_workflow_transcripts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let nested = temp.path().join("workflows").join("wf_nested");
    tokio::fs::create_dir_all(&nested)
        .await
        .expect("create nested transcript dir");
    let agent_id = lingxi_core::types::AgentId::new().to_string();
    let path = nested.join(format!("agent-{agent_id}.jsonl"));
    tokio::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({
            "message": lingxi_core::types::ConversationMessage::Assistant {
                per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "nested child".to_string(), citations: None,
                }],
                stop_reason: None,
                resumable: None,
            }
        }))
        .unwrap()
            + "\n",
    )
    .await
    .expect("write nested transcript");

    let paths = collect_session_agent_transcript_paths(temp.path())
        .await
        .expect("scan nested transcript tree");
    assert_eq!(paths, vec![path.clone()]);
    assert_eq!(
        find_session_agent_transcript_path(temp.path(), &agent_id)
            .await
            .expect("find nested transcript"),
        Some(path)
    );
}

#[test]
fn session_agent_transcript_revision_advances_for_hidden_compact_record() {
    let visible = lingxi_core::types::ConversationMessage::Assistant {
        per_turn_effort: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "visible".to_string(),
            citations: None,
        }],
        stop_reason: None,
        resumable: None,
    };
    let compact = lingxi_core::types::ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "replacement summary".to_string(),
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: true,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    };
    let first = serde_json::to_string(&serde_json::json!({"message": visible})).unwrap() + "\n";
    let second = first.clone()
        + &serde_json::to_string(&serde_json::json!({"message": compact})).unwrap()
        + "\n";
    assert_eq!(session_agent_transcript_revision(first.as_bytes()), 1);
    assert_eq!(session_agent_transcript_revision(second.as_bytes()), 2);
    assert_eq!(
        lower_session_agent_snapshot(first.as_bytes()).len(),
        lower_session_agent_snapshot(second.as_bytes()).len(),
        "hidden compact records may revise content without changing visible count"
    );
}

#[test]
fn main_session_agent_snapshot_joins_outer_uuids_to_stable_index_sidecar() {
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

    let first_id = MessageId::new();
    let api_row = lingxi_core::host::ServerFallbackApiErrorRow::new(
        &"declined fallback".into(),
        "2026-10-04T00:00:00.000Z".into(),
    );
    let first_message = ConversationMessage::Assistant {
        id: first_id,
        content: vec![ContentBlock::Text {
            text: "kept main row".into(),
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
        resumable: None,
        per_turn_effort: None,
    };
    let first_uuid = first_id.as_uuid().to_string();
    let api_uuid = api_row.uuid.as_uuid().to_string();
    let session_id = "22222222-3333-4333-8444-555555555555";
    let timestamp = "2026-10-04T00:00:00.000Z";
    let first_row = serde_json::json!({
        "type":"assistant",
        "uuid":first_uuid,
        "parentUuid":null,
        "sessionId":session_id,
        "timestamp":timestamp,
        "cwd":"/home/lingxi/lingxi",
        "version":"test",
        "isSidechain":false,
        "message":first_message
    });
    let attachment_row = serde_json::json!({
        "type":"attachment",
        "uuid":first_uuid,
        "parentUuid":null,
        "sessionId":session_id,
        "timestamp":timestamp,
        "cwd":"/home/lingxi/lingxi",
        "version":"test",
        "isSidechain":false,
        "message":first_message,
        "attachment":{"type":"test"}
    });
    let mut api_error_row = serde_json::to_value(&api_row).unwrap();
    let api_error_object = api_error_row
        .as_object_mut()
        .expect("Native API-error row is an object");
    api_error_object.insert("parentUuid".into(), serde_json::json!(first_uuid));
    api_error_object.insert("sessionId".into(), serde_json::json!(session_id));
    api_error_object.insert("cwd".into(), serde_json::json!("/home/lingxi/lingxi"));
    api_error_object.insert("version".into(), serde_json::json!("test"));
    api_error_object.insert("isSidechain".into(), serde_json::json!(false));
    let raw = [&first_row, &attachment_row, &api_error_row]
        .iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let raw = format!("{raw}\n");
    let identities = session::jsonl::SessionMessageIdentitySnapshot {
        by_uuid: HashMap::from([(first_uuid.clone(), 2), (api_uuid.clone(), 5)]),
        next_message_index: 6,
    };

    let replayed = replayed_session_from_jsonl(&raw);
    let rows = super::parse_main_session_agent_message_rows(&replayed, &identities).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "attachment sidecar is not a duplicate message row"
    );
    assert_eq!(rows[0].message_uuid, first_uuid);
    assert_eq!(rows[0].message_index, 2);
    assert_eq!(rows[1].message_uuid, api_uuid);
    assert_eq!(
        rows[1].message_index, 5,
        "deleted indices are not renumbered"
    );
    let decoded_api_error: lingxi_core::host::ServerFallbackApiErrorRow =
        serde_json::from_str(rows[1].api_error_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        api_error_row["message"]["stop_details"],
        serde_json::Value::Null
    );
    assert!(decoded_api_error
        .message
        .stop_details
        .as_ref()
        .is_some_and(|details| details.at_pointer("").is_some_and(|root| root.is_null())));
    let expected_persisted_api_error: lingxi_core::host::ServerFallbackApiErrorRow =
        serde_json::from_value(api_error_row.clone()).unwrap();
    assert_eq!(decoded_api_error, expected_persisted_api_error);
    assert_eq!(identities.next_message_index, 6);

    let mut duplicate_api_error_row = api_error_row.clone();
    let duplicate_api_uuid = MessageId::new().as_uuid().to_string();
    duplicate_api_error_row["uuid"] = serde_json::json!(duplicate_api_uuid);
    let ambiguous_raw = [
        first_row.clone(),
        attachment_row.clone(),
        api_error_row.clone(),
        duplicate_api_error_row,
    ]
    .iter()
    .map(|row| serde_json::to_string(row).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
    let ambiguous_replayed = replayed_session_from_jsonl(&format!("{ambiguous_raw}\n"));
    let ambiguous_identities = session::jsonl::SessionMessageIdentitySnapshot {
        by_uuid: HashMap::from([
            (first_uuid.clone(), 2),
            (api_uuid.clone(), 5),
            (duplicate_api_uuid, 6),
        ]),
        next_message_index: 7,
    };
    let error =
        super::parse_main_session_agent_message_rows(&ambiguous_replayed, &ambiguous_identities)
            .expect_err(
                "duplicate API-error inner IDs must not select an arbitrary outer identity",
            );
    assert!(error.contains("ambiguous API-error envelope identity"));

    let missing_identity = session::jsonl::SessionMessageIdentitySnapshot {
        by_uuid: HashMap::new(),
        next_message_index: 6,
    };
    let error = super::parse_main_session_agent_message_rows(&replayed, &missing_identity)
        .expect_err("a missing main-row identity must not be inferred from visible order");
    assert!(error.contains("stable identity index"));
}

#[test]
fn session_agent_transcript_event_is_dropped_after_session_switch() {
    let requested_session_id = lingxi_core::types::SessionId::new();
    let current_session_id = lingxi_core::types::SessionId::new();

    assert!(session_agent_transcript_event(
        requested_session_id,
        current_session_id,
        "agent:test".to_string(),
        Vec::new(),
        0,
        0,
    )
    .is_none());

    let event = session_agent_transcript_event(
        requested_session_id,
        requested_session_id,
        "agent:test".to_string(),
        Vec::new(),
        0,
        7,
    )
    .expect("same-session transcript event");
    let ClientEvent::SessionAgentTranscript {
        session_id,
        next_message_index,
        revision,
        ..
    } = event
    else {
        panic!("expected session-agent transcript event");
    };
    assert_eq!(session_id, requested_session_id.as_uuid().to_string());
    assert_eq!(next_message_index, 0);
    assert_eq!(revision, 7);
}

#[test]
fn session_agent_id_from_nested_transcript_path_requires_agent_jsonl_shape() {
    let agent_id = lingxi_core::types::AgentId::nil().to_string();
    let transcript_path = format!("/tmp/subagents/workflows/wf_1/agent-{agent_id}.jsonl");
    assert_eq!(
        super::session_agent_id_from_path(std::path::Path::new(&transcript_path)),
        Some(agent_id)
    );
    assert_eq!(
        super::session_agent_id_from_path(std::path::Path::new(
            "/tmp/subagents/workflows/wf_1/not-an-agent.txt"
        )),
        None
    );
}

#[test]
fn session_agent_index_excludes_hidden_transcript_records() {
    let hidden_meta = lingxi_core::types::ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "<runtime-reminder>internal</runtime-reminder>".to_string(),
            citations: None,
        }],
        is_meta: true,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    };
    let hidden_summary = lingxi_core::types::ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: Vec::new(),
        is_meta: false,
        is_compact_summary: true,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    };
    let hidden_transcript_only = lingxi_core::types::ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: Vec::new(),
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: true,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    };
    let visible = lingxi_core::types::ConversationMessage::Assistant {
        per_turn_effort: None,
        id: lingxi_core::types::MessageId::new(),
        content: Vec::new(),
        stop_reason: None,
        resumable: None,
    };
    assert!(!session_agent_conversation_is_visible(&hidden_meta));
    assert!(!session_agent_conversation_is_visible(&hidden_summary));
    assert!(!session_agent_conversation_is_visible(
        &hidden_transcript_only
    ));
    assert!(session_agent_conversation_is_visible(&visible));

    let raw = [hidden_meta, visible]
        .into_iter()
        .map(|message| serde_json::to_string(&serde_json::json!({ "message": message })).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let lowered = lower_session_agent_snapshot(raw.as_bytes());
    assert_eq!(
        lowered.len(),
        1,
        "meta seed must not occupy a live message index"
    );
    assert_eq!(lowered[0].role, "assistant");
}

#[tokio::test]
async fn session_agent_observer_binds_metadata_at_allocate_time() {
    let listener = Arc::new(FakeListener::default());
    let sink = ListenerSink::arc(listener.clone());
    let session_uuid = Arc::new(std::sync::Mutex::new("session-a".to_string()));
    let observer = MobileSessionAgentObserver::new(sink, session_uuid.clone());
    let agent_id = lingxi_core::types::AgentId::new();

    observer
        .on_event(SubagentObservation::Allocated {
            agent_id,
            agent_type: "researcher".to_string(),
            name: Some("Design".to_string()),
            model: "deepseek-flash".to_string(),
            model_profile: Some("deepseek".to_string()),
            persistent: false,
            initial_message_index: 0,
            origin_session_id: None,
        })
        .await;
    *session_uuid.lock().unwrap() = "session-b".to_string();
    observer
        .on_event(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::Assistant {
                per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "working".to_string(),
                    citations: None,
                }],
                stop_reason: None,
                resumable: None,
            },
        })
        .await;
    observer
        .on_event(SubagentObservation::Completed {
            agent_id,
            content: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""done""#)
                .expect("completed observation fixture is JSON"),
            usage: lingxi_core::host::SubagentUsage::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            assistant_message_count: 0,
            last_request_id: None,
        })
        .await;

    let events = listener.received.lock().await.clone();
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentUpdated { session_id, agent }
            if session_id == "session-a"
                && agent.agent_id == agent_id.to_string()
                && agent.name == ClientTextDto::from("Design")
                && agent.agent_type == "researcher"
                && agent.status == "running"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentMessage { session_id, agent_id: event_agent_id, row }
            if session_id == "session-a"
                && event_agent_id == &agent_id.to_string()
                && row.message_index == 0
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentUpdated { session_id, agent }
            if session_id == "session-a"
                && agent.agent_id == agent_id.to_string()
                && agent.name == ClientTextDto::from("Design")
                && agent.agent_type == "researcher"
                && agent.status == "completed"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentUpdated { session_id, .. }
            | ClientEvent::SessionAgentMessage { session_id, .. }
            if session_id == "session-b"
    )));
    assert!(observer.bound_agents.lock().await.is_empty());
    assert!(observer.tool_indexes.lock().await.is_empty());
    assert!(observer.message_indexes.lock().await.is_empty());
}

#[tokio::test]
async fn session_agent_observer_parking_preserves_binding_and_message_index() {
    let listener = Arc::new(FakeListener::default());
    let observer = MobileSessionAgentObserver::new(
        ListenerSink::arc(listener.clone()),
        Arc::new(std::sync::Mutex::new("session-a".to_string())),
    );
    let agent_id = lingxi_core::types::AgentId::new();
    observer
        .on_event(SubagentObservation::Allocated {
            agent_id,
            agent_type: "researcher".to_string(),
            name: Some("Research".to_string()),
            model: "deepseek-flash".to_string(),
            model_profile: None,
            persistent: false,
            initial_message_index: 3,
            origin_session_id: None,
        })
        .await;
    listener.received.lock().await.clear();

    observer
        .on_event(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::System {
                api_system: None,
                id: lingxi_core::types::MessageId::new(),
                subtype: Some("agent_idle".to_string()),
                content: "idle".into(),
                compact_metadata: None,
                model_fallback: None,
                refusal_fallback: None,
            },
        })
        .await;
    {
        let events = listener.received.lock().await;
        assert_eq!(events.len(), 1, "parking emits no visible message");
        assert!(matches!(
            &events[0],
            ClientEvent::SessionAgentUpdated { session_id, agent }
                if session_id == "session-a"
                    && agent.agent_id == agent_id.to_string()
                    && agent.status == "completed"
        ));
    }
    assert!(observer
        .bound_agents
        .lock()
        .await
        .contains_key(&agent_id.to_string()));
    assert_eq!(
        observer.message_indexes.lock().await[&agent_id.to_string()],
        3
    );

    observer
        .on_event(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::User {
                api_message_override: None,
                interrupted_by_shutdown: None,
                refused_message_id: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "follow-up".to_string(),
                    citations: None,
                }],
                is_meta: true,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
            },
        })
        .await;
    {
        let events = listener.received.lock().await;
        assert_eq!(events.len(), 2, "hidden wake emits only a status update");
        assert!(matches!(
            &events[1],
            ClientEvent::SessionAgentUpdated { agent, .. }
                if agent.status == "running" && agent.latest_activity.is_none()
        ));
    }
    assert_eq!(
        observer.message_indexes.lock().await[&agent_id.to_string()],
        3
    );

    observer
        .on_event(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::Assistant {
                per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "resumed".to_string(),
                    citations: None,
                }],
                stop_reason: None,
                resumable: None,
            },
        })
        .await;
    let events = listener.received.lock().await;
    assert_eq!(events.len(), 4);
    assert!(matches!(
        &events[2],
        ClientEvent::SessionAgentMessage {
            row: client::protocol::listings::SessionAgentMessageRowDto {
                message_index: 3,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        &events[3],
        ClientEvent::SessionAgentUpdated { agent, .. } if agent.status == "running"
    ));
}

#[tokio::test]
async fn session_agent_fallback_row_keeps_envelope_and_tombstone_leaves_index_gap() {
    let listener = Arc::new(FakeListener::default());
    let observer = MobileSessionAgentObserver::new(
        ListenerSink::arc(listener.clone()),
        Arc::new(std::sync::Mutex::new("session-a".to_string())),
    );
    let agent_id = lingxi_core::types::AgentId::new();
    observer
        .on_event(SubagentObservation::Allocated {
            agent_id,
            agent_type: "researcher".into(),
            name: Some("Research".into()),
            model: "test-model".into(),
            model_profile: None,
            persistent: true,
            initial_message_index: 0,
            origin_session_id: None,
        })
        .await;
    let mut api_error = lingxi_core::host::ServerFallbackApiErrorRow::new(
        &"fallback declined".into(),
        "2026-10-04T00:00:00.000Z".into(),
    );
    let refusal = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
        &serde_json::json!({"type":"refusal","category":"safety"}),
    )
    .expect("test refusal is valid JSON");
    api_error.set_refusal(Some("request-id".into()), refusal);
    observer
        .on_event(SubagentObservation::ServerFallbackApiErrorRow {
            agent_id,
            row: api_error.clone(),
            message_index: 0,
        })
        .await;
    observer
        .on_event(SubagentObservation::ServerFallbackTombstone {
            agent_id,
            message: lingxi_core::host::ServerFallbackTombstoneMessage {
                uuid: api_error.uuid,
                message_type: "assistant".into(),
                timestamp: api_error.timestamp.clone(),
                request_id: api_error.request_id.clone(),
                request_ref: None,
                provider_message_id: Some(api_error.message.id.as_uuid().to_string()),
                model: Some(api_error.message.model.clone()),
                stop_reason: Some(api_error.message.stop_reason.clone()),
                stop_details: api_error.message.stop_details.clone(),
                usage: Some(source_value(&api_error.message.usage)),
                content: api_error.message.content.clone(),
                is_api_error_message: Some(true),
                supersedes_uuids: None,
            },
            display_only: true,
        })
        .await;

    let events = listener.received.lock().await.clone();
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentMessage { row, .. }
            if row.message_index == 0
                && row.message_uuid == api_error.uuid.as_uuid().to_string()
                && row.api_error_json.as_deref().is_some_and(|json| {
                    serde_json::from_str::<serde_json::Value>(json)
                        .is_ok_and(|value| value["requestId"] == "request-id"
                            && value["message"]["stop_details"]["category"] == "safety")
                })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentTombstone { message_uuid, display_only: true, .. }
            if message_uuid == &api_error.uuid.as_uuid().to_string()
    )));
    let cache = observer.transcript_cache.lock().await;
    let snapshot = &cache[&agent_id.to_string()];
    assert!(snapshot.rows.is_empty());
    assert_eq!(snapshot.next_message_index, 1);
}

#[tokio::test]
async fn workflow_agent_observer_uses_pinned_origin_session() {
    let listener = Arc::new(FakeListener::default());
    let sink = ListenerSink::arc(listener.clone());
    let session_uuid = Arc::new(std::sync::Mutex::new("session-b".to_string()));
    let observer = MobileSessionAgentObserver::new(sink, session_uuid);
    let agent_id = lingxi_core::types::AgentId::new();
    let workflow_dir = std::path::PathBuf::from(
        "/profile/projects/workspace/session-a/subagents/workflows/wf_abcdef",
    );

    agent::with_transcript_subdir_override(Some(workflow_dir), async {
        observer
            .on_event(SubagentObservation::Allocated {
                agent_id,
                agent_type: "design".to_string(),
                name: Some("Design".to_string()),
                model: "deepseek-flash".to_string(),
                model_profile: Some("deepseek".to_string()),
                persistent: false,
                initial_message_index: 0,
                origin_session_id: None,
            })
            .await;
    })
    .await;

    assert!(listener.received.lock().await.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentUpdated { session_id, agent }
            if session_id == "session-a" && agent.agent_id == agent_id.to_string()
    )));
}

#[tokio::test]
async fn session_agent_summary_uses_metadata_type_when_name_is_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("agent-agent:test.jsonl");
    tokio::fs::write(
            &path,
            r#"{"agent_type":"researcher","model":"deepseek-flash","model_profile":"deepseek","status":"running"}
"#,
        )
        .await
        .expect("write transcript metadata");

    let summary = MobileEngineHandle::read_agent_summary("agent:test".to_string(), &path)
        .await
        .expect("summary");
    assert_eq!(summary.agent_type, "researcher");
    assert_eq!(summary.name, ClientTextDto::from("researcher"));
    assert_eq!(summary.model.as_deref(), Some("deepseek-flash"));
    assert_eq!(summary.model_profile.as_deref(), Some("deepseek"));
}

#[tokio::test]
async fn session_agent_summary_preserves_cancelled_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("agent-agent:test.jsonl");
    tokio::fs::write(
        &path,
        r#"{"agent_type":"researcher","status":"cancelled"}
"#,
    )
    .await
    .expect("write cancelled transcript metadata");

    let summary = MobileEngineHandle::read_agent_summary("agent:test".to_string(), &path)
        .await
        .expect("summary");
    assert_eq!(summary.status, "cancelled");
}

#[test]
fn android_recurring_schedule_enforces_fifteen_minute_floor() {
    assert!(mobile_cron_schedule_error("*/5 * * * *", true).is_some());
    assert!(mobile_cron_schedule_error("0,10 * * * *", true).is_some());
    assert!(mobile_cron_schedule_error("*/15 * * * *", true).is_none());
    assert!(mobile_cron_schedule_error("0 * * * *", true).is_none());
    assert!(mobile_cron_schedule_error("* * * * *", false).is_none());
    assert!(mobile_cron_schedule_error("61 * * * *", false).is_some());
}

#[test]
fn provider_connection_requires_selected_model_in_recognized_catalog() {
    let connected = classify_provider_connection_response(
        Ok(llm_runtime::services::sdk::directory::probe::ProbeResult {
            status: 200,
            model_ids: Some(vec!["deepseek-flash".into()]),
        }),
        "deepseek-flash",
        42,
        true,
    );
    assert!(connected.connected);
    assert!(connected.authenticated);
    assert!(connected.model_available);
    assert_eq!(Some(200), connected.http_status);
    assert!(connected.used_stored_credential);

    let missing = classify_provider_connection_response(
        Ok(llm_runtime::services::sdk::directory::probe::ProbeResult {
            status: 200,
            model_ids: Some(vec!["gemini-2.5-flash".into()]),
        }),
        "gemini-2.5-pro",
        9,
        false,
    );
    assert!(!missing.connected);
    assert!(missing.authenticated);
    assert!(!missing.model_available);
    assert!(!missing.used_stored_credential);
}

#[test]
fn provider_connection_maps_auth_failure_without_echoing_response_body() {
    let result = classify_provider_connection_response(
        Err(lingxi_core::host::HttpError::Status {
            status: 401,
            body: "secret-bearing upstream response".to_string(),
        }),
        "deepseek-flash",
        18,
        true,
    );

    assert!(!result.connected);
    assert!(result.reachable);
    assert!(!result.authenticated);
    assert_eq!(Some(401), result.http_status);
    assert!(!result.message.contains("upstream"));
    assert!(!result.message.contains("secret"));
}

#[test]
fn provider_connection_maps_forbidden_rate_limit_and_timeout_without_secrets() {
    for (response, status, expected_fragment) in [
        (
            Err(lingxi_core::host::HttpError::Status {
                status: 403,
                body: "private upstream detail".to_string(),
            }),
            Some(403),
            "拒绝访问",
        ),
        (
            Err(lingxi_core::host::HttpError::Status {
                status: 429,
                body: "retry-after: 30".to_string(),
            }),
            Some(429),
            "频率",
        ),
        (
            Err(lingxi_core::host::HttpError::Timeout(
                std::time::Duration::from_secs(1),
            )),
            None,
            "超时",
        ),
    ] {
        let result = classify_provider_connection_response(response, "model", 20, true);
        assert!(!result.connected);
        assert_eq!(result.http_status, status);
        assert!(result.message.contains(expected_fragment));
        assert!(!result.message.contains("private"));
        assert!(!result.message.contains("secret"));
    }
}

#[tokio::test]
async fn lightweight_cron_store_crud_and_due_occurrence_need_no_engine() {
    use platform_posix_minimal::{PosixClock, PosixFileSystem};

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join(branding::DOT_DIR)).expect("state dir");
    let store = MobileCronStoreHandle::new(
        temp.path().to_path_buf(),
        Arc::new(PosixFileSystem::new(temp.path().to_path_buf())),
        Arc::new(PosixClock::new()),
    );

    let created = store
        .create("* * * * *".to_string(), "hello".to_string(), false)
        .await
        .expect("one-shot creation");
    assert_eq!(1, store.list().await.len());
    let updated = store
        .update(
            created.id.clone(),
            "*/15 * * * *".to_string(),
            "updated".to_string(),
            true,
        )
        .await
        .expect("recurring update");
    assert_eq!("updated", updated.prompt);
    let due = store
        .due_occurrences(updated.next_fire_ms.expect("next fire").saturating_add(1))
        .await;
    assert_eq!(created.id, due[0].task_id);
    assert!(store.delete(created.id).await);
    assert!(store.list().await.is_empty());
}

/// In-memory encrypted store used to exercise post-boot provider credential
/// writes without involving a platform keychain.
#[derive(Default)]
struct FakeEncryptedStore {
    map: StdMutex<HashMap<(String, String), lingxi_core::types::SecureStorageData>>,
}

#[async_trait]
impl lingxi_core::host::SecureStorage for FakeEncryptedStore {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: lingxi_core::types::SecureStorageData,
    ) -> Result<(), lingxi_core::host::SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .insert((service.to_string(), account.to_string()), data);
        Ok(())
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<lingxi_core::types::SecureStorageData>, lingxi_core::host::SecureStorageError>
    {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    async fn delete(
        &self,
        service: &str,
        account: &str,
    ) -> Result<(), lingxi_core::host::SecureStorageError> {
        self.map
            .lock()
            .unwrap()
            .remove(&(service.to_string(), account.to_string()));
        Ok(())
    }

    async fn list(
        &self,
        service: &str,
    ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .keys()
            .filter(|(stored_service, _)| stored_service == service)
            .map(|(_, account)| account.clone())
            .collect())
    }

    fn is_encrypted(&self) -> bool {
        true
    }

    fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
        lingxi_core::host::SecureStorageBackend::EncryptedFile
    }
}

/// F3-03: `MobileConfig::default` is constructible and its frozen field set
/// is reachable — the mobile analog of `desktop_config_default_is_constructible`.
#[test]
fn mobile_config_default_is_constructible() {
    let cfg = MobileConfig::default();
    assert_eq!(cfg.api_base, "https://api.anthropic.com");
    assert!(cfg.api_key.is_empty());
    assert_eq!(cfg.cwd, std::path::PathBuf::from("."));
    assert_eq!(cfg.default_model, "anthropic/claude-sonnet-5");
    // The boot default must be a CURATED Anthropic id, so a client with no
    // configured provider lands inside the shortlist its picker renders.
    assert!(lingxi_core::host::is_curated_model(
        "anthropic",
        cfg.default_model.rsplit('/').next().unwrap_or_default()
    ));
    assert!(cfg.provider_profiles.is_none());
    assert!(cfg.routing.is_none());
    // P0.2: the injectable memory provider defaults to None (empty,
    // deterministic — production injects `Some(real_provider())`).
    assert!(cfg.memory_provider.is_none());
    let _clone = cfg.clone();
    // Exercises the manual `Debug` impl that renders `memory_provider` as a
    // presence marker (`Arc<dyn MemoryHierarchyProvider>` is not `Debug`).
    let _ = format!("{cfg:?}");
}

/// F3-03: `build_mobile` constructs a real `ConversationOrchestrator`
/// off-device, from a `MobileConfig` + a host fake `Platform` alone — no
/// `std::env`, no device. This is the mobile sibling of
/// `build_constructs_runtime_deterministically`.
#[tokio::test]
async fn build_mobile_constructs_orchestrator() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());

    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile failed");

    // The orchestrator exists and exposes the `OrchestratorHandle` surface
    // the command registry binds to. Constructing it at all proves the full
    // mobile assembly (tool registry + command registry + adapter sinks).
    let _handle: Arc<dyn lingxi_core::host::OrchestratorHandle> = rt.orchestrator.clone();

    // SKILLLIST.1: the production composition must attach the listing
    // provider before the orchestrator is wrapped.
    assert!(
        rt.orchestrator.has_skill_listing(),
        "mobile orchestrator must expose a skill-listing provider"
    );
}

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(".lingxi").join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("create skill dir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\ndescription: {description}\n---\n{body}\n"),
    )
    .expect("write skill");
}

#[tokio::test]
async fn mobile_listing_dispatcher_and_skill_tool_share_one_live_registry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_skill(tmp.path(), "foo", "Foo skill", "FOO BODY v1");
    let commands_dir = tmp.path().join(".lingxi").join("commands");
    std::fs::create_dir_all(&commands_dir).expect("create commands dir");
    std::fs::write(
        commands_dir.join("loop.md"),
        "---\ndescription: Decoy loop\n---\nDECOY LOOP BODY\n",
    )
    .expect("write loop decoy");

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile failed");

    // D1: interrogate the handles `build_mobile` ACTUALLY wired — the
    // provider the orchestrator reads for its per-turn skill listing and
    // the loader the Skill tool calls — instead of building fresh ones over
    // `rt.slash_registry`. Rebuilt handles would only prove that a provider
    // over the shared registry works; they would stay green if the
    // composition root handed either surface a registry of its own, leaving
    // the model's skill listing permanently empty on device while slash
    // invocation kept working.
    let provider = rt.wired_skill_listing_provider.clone();
    let loader = rt.wired_skill_loader.clone();

    let listed = provider
        .skill_listing_snapshot()
        .await
        .expect("mobile skill listing snapshot")
        .entries;
    let listed_names: std::collections::BTreeSet<_> = listed
        .iter()
        .map(|entry| entry.name.string_at("").expect("skill name is a string"))
        .collect();
    let expected = vec!["loop".to_string(), "foo".to_string()];
    for name in &expected {
        assert!(
            listed_names.contains(name.as_str()),
            "live mobile listing must contain {name:?}: {listed_names:?}"
        );
        let desc = loader
            .load(&source_text(name))
            .await
            .expect("load ok")
            .unwrap_or_else(|| panic!("listed skill {name:?} must resolve through Skill"));
        assert_eq!(
            desc.command_type,
            SkillCommandType::Prompt,
            "listed entry {name:?} must remain prompt-invocable"
        );
    }
    let foo_v1 = loader
        .load(&source_text("foo"))
        .await
        .expect("load ok")
        .expect("foo present");
    assert!(source_string_contains(&foo_v1.body, "FOO BODY v1"));
    let loop_desc = loader
        .load(&source_text("loop"))
        .await
        .expect("load ok")
        .expect("loop present");
    let loop_prompt = loop_desc
        .dynamic_body
        .as_ref()
        .expect("bundled loop stays programmatic")
        .build(&source_text(""));
    assert!(
        !source_string_contains(&loop_prompt, "DECOY LOOP BODY"),
        "same-name disk decoy must not override bundled loop"
    );
    let registry = rt.slash_registry.read().await;
    let resolved_loop = registry
        .resolve(&source_text("loop"))
        .expect("loop resolves");
    assert_eq!(
        resolved_loop.loaded_from.as_deref(),
        Some("bundled"),
        "loop must resolve from bundled after boot restore"
    );
    drop(registry);
    let listed_loop = listed
        .iter()
        .find(|entry| entry.name.string_at("") == Some("loop"))
        .expect("loop listed");
    assert!(listed_loop.is_bundled_prompt, "loop must list as bundled");

    write_skill(tmp.path(), "foo", "Foo skill", "FOO BODY v2");
    match rt
        .dispatcher
        .dispatch(
            source_text("/reload-skills"),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    {
        SlashDispatchResult::Handled { .. } => {}
        other => panic!("reload-skills must be handled locally, got {other:?}"),
    }
    let foo_v2 = loader
        .load(&source_text("foo"))
        .await
        .expect("load ok")
        .expect("foo present after reload");
    assert!(source_string_contains(&foo_v2.body, "FOO BODY v2"));
    let loop_after_reload = loader
        .load(&source_text("loop"))
        .await
        .expect("load ok")
        .expect("loop still present after reload");
    let loop_prompt_after_reload = loop_after_reload
        .dynamic_body
        .as_ref()
        .expect("bundled loop stays programmatic")
        .build(&source_text(""));
    assert!(
        !source_string_contains(&loop_prompt_after_reload, "DECOY LOOP BODY"),
        "bundled loop must survive reload precedence"
    );

    std::fs::remove_dir_all(tmp.path().join(".lingxi").join("skills").join("foo"))
        .expect("remove foo skill");
    match rt
        .dispatcher
        .dispatch(
            source_text("/reload-skills"),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    {
        SlashDispatchResult::Handled { .. } => {}
        other => panic!("reload-skills must be handled locally, got {other:?}"),
    }
    let listed_after_delete = provider
        .skill_listing_snapshot()
        .await
        .expect("mobile skill listing snapshot")
        .entries;
    let listed_after_delete_names: std::collections::BTreeSet<_> = listed_after_delete
        .iter()
        .map(|entry| entry.name.string_at("").expect("skill name is a string"))
        .collect();
    assert!(
        !listed_after_delete_names.contains("foo"),
        "deleted disk skill must disappear from live listing: {listed_after_delete_names:?}"
    );
    assert!(
        loader
            .load(&source_text("foo"))
            .await
            .expect("load ok")
            .is_none(),
        "deleted disk skill must disappear from the shared Skill loader"
    );
    let loop_after_delete = loader
        .load(&source_text("loop"))
        .await
        .expect("load ok")
        .expect("loop still present after delete reload");
    let loop_prompt_after_delete = loop_after_delete
        .dynamic_body
        .as_ref()
        .expect("bundled loop stays programmatic")
        .build(&source_text(""));
    assert!(
        !source_string_contains(&loop_prompt_after_delete, "DECOY LOOP BODY"),
        "bundled loop precedence must survive repeated reloads"
    );
    let registry_after_delete = rt.slash_registry.read().await;
    let resolved_loop_after_delete = registry_after_delete
        .resolve(&source_text("loop"))
        .expect("loop resolves after reload");
    assert_eq!(
        resolved_loop_after_delete.loaded_from.as_deref(),
        Some("bundled"),
        "loop must still resolve from bundled after repeated reloads"
    );
    drop(registry_after_delete);
}

#[tokio::test]
async fn mobile_bundled_device_skill_is_prompt_discoverable_after_executor_conversion() {
    let mut registry = command_api::CommandRegistry::new();
    crate::mobile::device_skills::register_mobile_device_skills(
        &mut registry,
        &["device_status".to_owned()],
    );
    let mut device_skill = registry
        .resolve(&source_text("mobile-device"))
        .expect("mobile device skill is registered")
        .clone();
    device_skill.user_facing_name = Some("device-short".into());
    registry.register_command(device_skill);
    registry.register_command(command_api::SlashCommand {
        name: "device-short".into(),
        description: "disabled alias collision".into(),
        source: command_api::CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        kind: command_api::SlashCommandKind::Markdown {
            file_path: std::path::PathBuf::from("disabled-device-short.md"),
            frontmatter: command_api::CommandFrontmatter::default(),
            prompt_template: "hidden".into(),
        },
        is_enabled: false,
        native_type: Some(command_api::NativeCommandType::Prompt),
        has_user_specified_description: true,
        loaded_from: Some("skills".to_owned()),
        ..command_api::SlashCommand::default()
    });
    registry.register_command(command_api::SlashCommand {
        name: "disabled-mobile-skill".into(),
        description: "disabled fixture".into(),
        source: command_api::CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        kind: command_api::SlashCommandKind::Markdown {
            file_path: std::path::PathBuf::from("disabled-mobile-skill.md"),
            frontmatter: command_api::CommandFrontmatter::default(),
            prompt_template: "hidden".into(),
        },
        is_enabled: false,
        native_type: Some(command_api::NativeCommandType::Prompt),
        has_user_specified_description: true,
        loaded_from: Some("skills".to_owned()),
        ..command_api::SlashCommand::default()
    });
    let device_skill = registry
        .resolve(&source_text("mobile-device"))
        .expect("mobile device skill is registered");
    assert!(device_skill.is_prompt());
    assert!(matches!(
        &device_skill.kind,
        command_api::SlashCommandKind::Bundled { .. }
    ));

    // Exercise the production Mobile listing mapper over the actual bundled
    // device-skill producer. Its dispatch kind is Bundled, so Native prompt
    // classification must come from the original markdown factory.
    let provider = super::mobile_skill_listing_provider_with_settings(
        Arc::new(RwLock::new(registry)),
        session::jsonl::SessionMode::Code,
        None,
        Arc::new(|| Ok(false)),
        None,
        None,
    );
    let snapshot = provider
        .skill_listing_snapshot()
        .await
        .expect("mobile skill listing snapshot");
    assert!(snapshot
        .entries
        .iter()
        .all(|entry| { entry.name.string_at("") != Some("disabled-mobile-skill") }));
    let entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.name.string_at("") == Some("mobile-device"))
        .expect("bundled mobile-device prompt must reach the model-facing listing");
    assert!(entry.is_bundled_prompt);
    assert_eq!(
        entry.source.as_ref().and_then(|value| value.string_at("")),
        Some("built-in")
    );
    assert_eq!(
        entry.display_name.string_at(""),
        Some("mobile-device (device-short)"),
        "disabled command names must not shadow Native's full-catalog display-name resolution"
    );
}

/// Write a minimal fixture plugin directory `root/{plugin_name}` with one
/// namespaced command (`commands/{cmd_name}.md`) and one namespaced agent
/// (`agents/{agent_name}.md`), in the exact on-disk shape
/// `plugin::discovery::discover_installed_plugins` auto-detects (mirrors
/// `plugin::manager::agent_privilege_tests::write_single_agent_plugin`).
fn write_plugin_fixture(
    root: &Path,
    plugin_name: &str,
    cmd_name: &str,
    cmd_body: &str,
    agent_name: &str,
) {
    let plugin_dir = root.join(plugin_name);
    std::fs::create_dir_all(plugin_dir.join(".lingxi-plugin")).expect("plugin manifest dir");
    std::fs::write(
        plugin_dir.join(".lingxi-plugin").join("plugin.json"),
        format!(r#"{{"name":"{plugin_name}","version":"1.0.0"}}"#),
    )
    .expect("write plugin.json");
    std::fs::create_dir_all(plugin_dir.join("commands")).expect("commands dir");
    std::fs::write(
        plugin_dir.join("commands").join(format!("{cmd_name}.md")),
        format!("---\ndescription: fixture command\n---\n{cmd_body}\n"),
    )
    .expect("write fixture command");
    std::fs::create_dir_all(plugin_dir.join("agents")).expect("agents dir");
    std::fs::write(
        plugin_dir.join("agents").join(format!("{agent_name}.md")),
        format!("---\nname: {agent_name}\ndescription: fixture agent\n---\nI am a fixture.\n"),
    )
    .expect("write fixture agent");
}

/// P1.8 (§19.2) — the required gate: the command, skill, and agent
/// surfaces `plugin::PluginManager` materializes into must be the SAME
/// live objects the model's listing and invocation paths already read —
/// not copies seeded with equal content — and the pre-existing workflow
/// task registry must remain the one object both the `Workflow` tool and
/// the `/workflows` listing command share.
///
/// Every check below is discriminating, not merely descriptive: each one
/// is answered by mutating state through ONE named surface and observing
/// the change through a DIFFERENT, independently-retained handle. Two
/// registries seeded with identical starting content (see
/// `two_separately_allocated_catalogs_with_equal_content_are_not_the_same_registry`
/// below) would satisfy every assertion here UNTIL the mutation step,
/// where only genuine identity keeps them in sync.
#[tokio::test]
async fn listing_and_invocation_share_one_registry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plugin_name = "p18-fixture-plugin";
    let cmd_name = "hello";
    let agent_name = "helper";
    write_plugin_fixture(
        tmp.path(),
        plugin_name,
        cmd_name,
        "FIXTURE COMMAND BODY",
        agent_name,
    );

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile failed");

    // --- Workflow leg: pre-existing sharing, asserted here so all four
    // §19.2 surfaces are covered by one gate. `WorkflowsHandler` (the
    // `/workflows` listing command) and `MobileWorkflowLauncher` (what the
    // `Workflow` tool actually invokes through) must read the SAME
    // `TaskRegistry`, or a run the tool starts could go unlisted, or a
    // listed run could be unreachable to invoke against.
    assert!(
        Arc::ptr_eq(&rt.task_registry, &rt.workflow_launcher.registry),
        "the Workflow tool's launcher and the /workflows listing command must \
             share one live TaskRegistry, not two separately-constructed ones"
    );

    // --- Command/skill leg: materialize a REAL fixture plugin through the
    // VERY `PluginManager` `build_mobile` composed (never a manager the
    // test builds itself, and never `register_verified_builtin` — that
    // symbol's one production call site stays in `lib.rs`).
    let discovered = plugin::discovery::discover_installed_plugins(tmp.path()).await;
    let (id, manifest, install_dir) = discovered
        .into_iter()
        .find(|(_, m, _)| m.name == plugin_name)
        .expect("fixture plugin discovered on disk");
    rt.wired_plugin_manager
        .enable(&id, manifest, install_dir)
        .await
        .expect("fixture plugin must enable cleanly");

    let namespaced_cmd = format!("{plugin_name}:{cmd_name}");
    let namespaced_agent = format!("{plugin_name}:{agent_name}");

    // Listing: the model's per-turn skill/command listing must now name
    // the plugin's command — read through `wired_skill_listing_provider`,
    // the VERY handle the orchestrator's per-turn prompt reads, not a
    // provider the test builds fresh over `rt.slash_registry`.
    let listed = rt
        .wired_skill_listing_provider
        .skill_listing_snapshot()
        .await
        .expect("wired mobile skill listing snapshot")
        .entries;
    assert!(
        listed
            .iter()
            .any(|entry| entry.name.string_at("") == Some(namespaced_cmd.as_str())),
        "the plugin's command must appear in the live listing after \
             PluginManager::enable: {:?}",
        listed
            .iter()
            .map(|entry| entry.name.string_at("").unwrap_or_default())
            .collect::<Vec<_>>()
    );
    let plugin_entry = listed
        .iter()
        .find(|entry| entry.name.string_at("") == Some(namespaced_cmd.as_str()))
        .expect("plugin prompt appears in the live listing");
    assert_eq!(
        plugin_entry
            .source
            .as_ref()
            .and_then(|value| value.string_at("")),
        Some("plugin")
    );
    assert_eq!(
        plugin_entry
            .plugin_name
            .as_ref()
            .and_then(|value| value.string_at("")),
        Some(plugin_name)
    );

    // Invocation: the Skill tool's OWN loader — `wired_skill_loader` —
    // must resolve the identical name to the identical body. If
    // `PluginManager` had been handed a `CommandRegistry` of its own
    // instead of `shared_command_registry`, the listing check above and
    // this one could both still fail (or, worse, only one of them would),
    // which is exactly the split §19.2 forbids.
    let loaded = rt
        .wired_skill_loader
        .load(&source_text(&namespaced_cmd))
        .await
        .expect("load ok")
        .unwrap_or_else(|| panic!("plugin command {namespaced_cmd:?} must be invocable"));
    assert!(
        source_string_contains(&loaded.body, "FIXTURE COMMAND BODY"),
        "invoked body must be the fixture plugin command's own content: {:?}",
        loaded.body
    );

    // --- Agent leg: PluginManager's `agent_catalog` must be the exact
    // object the real subagent spawner's set-once cell was filled with,
    // not a second catalog that merely started with equal builtin
    // content.
    let via_spawner_cell = rt
        .wired_subagent_agent_catalog_cell
        .get()
        .expect("subagent spawner's agent-catalog cell must be filled by boot")
        .clone();
    assert!(
        Arc::ptr_eq(&via_spawner_cell, &rt.wired_agent_catalog),
        "PluginManager's agent_catalog and the subagent spawner's live \
             catalog must be the SAME Arc allocation, not two catalogs seeded \
             with equal content"
    );
    // Listing surface: PluginManager's own catalog (mutated by `enable`
    // above) must already show the fixture agent.
    let listing_names: Vec<String> = rt
        .wired_agent_catalog
        .read()
        .await
        .iter()
        .map(|def| def.agent_type.clone())
        .collect();
    assert!(
        listing_names.contains(&namespaced_agent),
        "the plugin's agent must be present in PluginManager's live catalog: \
             {listing_names:?}"
    );
    // Invocation surface: read through the SPAWNER'S OWN cell — a
    // genuinely independent handle from `wired_agent_catalog` above —
    // proving the mutation `enable()` made is visible on the invocation
    // path itself, not merely on the handle the test happened to mutate
    // through.
    let invocation_names: Vec<String> = via_spawner_cell
        .read()
        .await
        .iter()
        .map(|def| def.agent_type.clone())
        .collect();
    assert!(
        invocation_names.contains(&namespaced_agent),
        "the plugin's agent must be resolvable through the subagent \
             spawner's own catalog handle: {invocation_names:?}"
    );
}

/// House-defect guard for the test above: prove the discriminating
/// assertions above are actually discriminating. Two catalogs built from
/// the SAME seed content (`agent::builtins::builtin_agent_definitions()`)
/// are NOT `Arc::ptr_eq`, and mutating one is invisible through the
/// other — the exact failure shape `listing_and_invocation_share_one_registry`
/// exists to catch, reproduced here in isolation without booting a whole
/// `MobileRuntime`.
#[tokio::test]
async fn two_separately_allocated_catalogs_with_equal_content_are_not_the_same_registry() {
    let listing: Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>> = Arc::new(
        tokio::sync::RwLock::new(agent::builtins::builtin_agent_definitions()),
    );
    let invocation: Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>> = Arc::new(
        tokio::sync::RwLock::new(agent::builtins::builtin_agent_definitions()),
    );

    // Equal content at construction — this is the trap: a value-equality
    // assertion here would pass despite these being two independent
    // allocations.
    assert_eq!(
        listing.read().await.len(),
        invocation.read().await.len(),
        "fixture setup: both catalogs must start with equal content"
    );
    assert!(
        !Arc::ptr_eq(&listing, &invocation),
        "two separately Arc::new-allocated catalogs must never be ptr_eq, \
             even with identical content"
    );

    // Mutate ONLY `listing` (as `PluginManager::enable` would through
    // whichever catalog it was actually wired to) and confirm the
    // "invocation" surface never sees it — the discriminating behavior
    // `listing_and_invocation_share_one_registry` depends on to fail loud
    // if a future refactor re-splits the two catalogs.
    let planted = agent::builtins::builtin_agent_definitions()
        .into_iter()
        .next()
        .expect("at least one builtin agent definition exists")
        .clone();
    let mut planted = planted;
    planted.agent_type = "planted:only-in-listing".to_string();
    listing.write().await.push(planted.clone());

    assert!(
        listing
            .read()
            .await
            .iter()
            .any(|def| def.agent_type == planted.agent_type),
        "the mutation must actually have landed in `listing`"
    );
    assert!(
        !invocation
            .read()
            .await
            .iter()
            .any(|def| def.agent_type == planted.agent_type),
        "a mutation through `listing` must NEVER appear in a separately-\
             allocated `invocation` catalog — if it does, this fixture no \
             longer demonstrates the failure shape the real gate depends on"
    );
}

/// Audit (secure-storage): the runtime's `oauth_supported` reflects the
/// platform's injected secure store — `false` with the non-persisting stub
/// (so `/login` is gated off), `true` once a real encrypted store is injected
/// (enabling the OAuth persist path). Proves the native secure-storage
/// injection seam (`Platform::secure_storage()` → `build_mobile_inner`)
/// end-to-end, off-device.
#[tokio::test]
async fn injected_encrypted_store_enables_oauth() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // No store injected → the non-persisting stub → OAuth /login gated off.
    let rt_stub = build_mobile(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .await
    .expect("build_mobile (stub) failed");
    assert!(
        !rt_stub.oauth_supported,
        "the non-persisting stub store must gate OAuth /login off"
    );
    assert!(
        !rt_stub.mcp_registry.has_oauth(),
        "plaintext fallback must not wire MCP OAuth dependencies"
    );

    // Inject an encrypted store → OAuth /login enabled.
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_secure_storage(Arc::new(FakeEncryptedStore::default())),
    );
    let rt_real = build_mobile(
        test_config(tmp.path()),
        platform,
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .await
    .expect("build_mobile (encrypted) failed");
    assert!(
        rt_real.oauth_supported,
        "an injected encrypted secure store must enable OAuth /login"
    );
    assert!(
        rt_real.mcp_registry.has_oauth(),
        "encrypted secure storage must wire MCP OAuth dependencies"
    );
}

#[tokio::test]
async fn mobile_build_returns_before_interactive_mcp_oauth() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(tmp.path());
    std::fs::create_dir_all(&cfg.lingxi_home).expect("create mobile home");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging MCP server");
    let address = listener.local_addr().expect("MCP server address");
    let server_task = tokio::spawn(async move {
        let mut streams = Vec::new();
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            // Keep each request open so an OAuth/negotiation path that is
            // accidentally awaited by build would visibly hang this test.
            streams.push(stream);
        }
    });
    std::fs::write(
            cfg.lingxi_home.join("settings.json"),
            format!(
                r#"{{"mcpServers":{{"oauth-remote":{{"type":"http","url":"http://{address}/mcp","oauth":{{"clientId":"mobile-test"}}}}}}}}"#
            ),
        )
        .expect("write MCP config");
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_secure_storage(Arc::new(FakeEncryptedStore::default())),
    );
    let started = std::time::Instant::now();
    let runtime = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        build_mobile(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
        ),
    )
    .await
    .expect("mobile build must not wait for OAuth interaction")
    .expect("mobile build failed");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "configured remote connect must be backgrounded"
    );
    assert!(runtime.oauth_supported);
    assert!(runtime.mcp_registry.has_oauth());
    server_task.abort();
}

#[test]
fn mobile_mcp_boot_and_reload_preflight_reject_plaintext_oauth_before_dial() {
    let oauth = lingxi_core::host::McpOAuthConfigDto {
        client_id: Some("mobile-test".into()),
        callback_port: None,
        auth_server_metadata_url: None,
        scopes: None,
        xaa: None,
    };
    let config = McpServerConfig {
        name: "remote".into(),
        spec: lingxi_core::host::McpTransportSpec::Http {
            url: "https://127.0.0.1:1/mcp".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: Some(oauth),
        },
        scope: McpConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        discovery_cache: None,
        config_error: None,
        metadata: Default::default(),
    };
    let boot = mobile_mcp_preflight(vec![config.clone()], false);
    let reload = mobile_mcp_preflight(vec![config], false);
    let expected = Some("MCP OAuth requires an encrypted secure credential store".to_string());
    assert_eq!(boot[0].config_error, expected);
    assert_eq!(reload[0].config_error, expected);
}

#[test]
fn mobile_mcp_reload_snapshot_is_complete_and_stable() {
    let mut headers = lingxi_core::host::McpHeaders::new();
    headers.insert("X-First".into(), "one".into());
    headers.insert("X-Second".into(), "two".into());
    let config = McpServerConfig {
        name: "remote".into(),
        spec: lingxi_core::host::McpTransportSpec::Http {
            url: "https://example.test/mcp".into(),
            headers,
            headers_helper: Some("helper".into()),
            oauth: None,
        },
        scope: McpConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: Some(5000),
        always_load: true,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        discovery_cache: Some(true),
        config_error: None,
        metadata: Default::default(),
    };
    let same = config.clone();
    assert!(mobile_mcp_config_unchanged(&config, &same));

    let mut changed = config.clone();
    if let lingxi_core::host::McpTransportSpec::Http { headers, .. } = &mut changed.spec {
        headers.insert("X-Third".into(), "three".into());
    }
    assert!(!mobile_mcp_config_unchanged(&config, &changed));

    let mut reordered = config.clone();
    if let lingxi_core::host::McpTransportSpec::Http { headers, .. } = &mut reordered.spec {
        let first = headers.shift_remove("X-First").unwrap();
        headers.insert("X-First".into(), first);
    }
    assert!(
        !mobile_mcp_config_unchanged(&config, &reordered),
        "header order is part of MCP server identity"
    );
}

struct RecordingDeepLinkOpener {
    opened: Arc<StdMutex<Vec<String>>>,
    succeed: bool,
}

#[async_trait]
impl lingxi_core::host::DeepLinkOpener for RecordingDeepLinkOpener {
    async fn open(&self, url: String) -> Result<(), lingxi_core::host::DeepLinkError> {
        self.opened.lock().unwrap().push(url);
        if self.succeed {
            Ok(())
        } else {
            Err(lingxi_core::host::DeepLinkError::Unavailable)
        }
    }
}

#[tokio::test]
async fn mobile_mcp_authorization_url_slot_survives_open_success_failure_and_absence() {
    let success_slot = Arc::new(StdMutex::new(None));
    let success_opened = Arc::new(StdMutex::new(Vec::new()));
    let success_opener = Arc::new(RecordingDeepLinkOpener {
        opened: success_opened.clone(),
        succeed: true,
    });
    let success_callback =
        mobile_mcp_oauth_authorization_callback(success_slot.clone(), Some(success_opener));
    success_callback("https://auth.example/success");
    for _ in 0..100 {
        if success_opened.lock().unwrap().len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        success_slot.lock().unwrap().as_deref(),
        Some("https://auth.example/success")
    );
    assert_eq!(
        success_opened.lock().unwrap().first().map(String::as_str),
        Some("https://auth.example/success")
    );

    let failure_slot = Arc::new(StdMutex::new(None));
    let failure_opened = Arc::new(StdMutex::new(Vec::new()));
    let failure_callback = mobile_mcp_oauth_authorization_callback(
        failure_slot.clone(),
        Some(Arc::new(RecordingDeepLinkOpener {
            opened: failure_opened.clone(),
            succeed: false,
        })),
    );
    failure_callback("https://auth.example/failure");
    for _ in 0..100 {
        if failure_opened.lock().unwrap().len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        failure_slot.lock().unwrap().as_deref(),
        Some("https://auth.example/failure")
    );
    assert_eq!(failure_opened.lock().unwrap().len(), 1);

    let absent_slot = Arc::new(StdMutex::new(None));
    let absent_callback = mobile_mcp_oauth_authorization_callback(absent_slot.clone(), None);
    absent_callback("https://auth.example/copy");
    assert_eq!(
        absent_slot.lock().unwrap().as_deref(),
        Some("https://auth.example/copy")
    );
}

/// F3-03: the built runtime binds the adapter sinks — the
/// `AdapterPermissionGate` (proven by parking a real `check()` that lands on
/// the recording sink) and the listener-backed output stream (proven by the
/// returned listener being the one we registered).
#[tokio::test]
async fn mobile_runtime_binds_adapter_sinks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let sink = Arc::new(RecordingPermissionSink::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = sink.clone();

    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile failed");

    // The policy gate is the same enforcing gate injected into the mobile
    // builtin tool context. A headless ExitPlanMode check must deny before
    // it can mutate plan state rather than relying on a prompt transport.
    let headless_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
        is_non_interactive_session: true,
        ..Default::default()
    };
    let outcome = lingxi_core::host::permission_gate::PermissionGate::check_exit_plan_mode(
        rt.permission_policy_gate.as_ref(),
        "1. Ship it",
        &headless_ctx,
    )
    .await;
    assert!(matches!(
        outcome,
        lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason }
            if source_string_starts_with(&reason, "Permission to use ExitPlanMode has been denied.")
    ));

    // Drive a `check()` on a spawned task; a deny-by-default tool parks a
    // request on the sink (proving the adapter gate is bound, not a no-op).
    let gate = rt.permission_gate.clone();
    let g = gate.clone();
    let task = tokio::spawn(async move {
        use permission::gate::PermissionGate;
        g.check("Bash", &source_value(&serde_json::json!({"command": "ls"})))
            .await
    });

    for _ in 0..2000 {
        if sink.count.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sink.count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "adapter gate must emit a PermissionRequest"
    );

    // Resolve so the parked future returns (request id starts at 1).
    assert!(
        gate.resolve(
            1,
            client::protocol::permission::PermissionResponseDto::Deny,
            "Bash"
        )
        .await
    );
    let _ = task.await.unwrap();
}

// ── P0.2: mobile hook lifecycle (real HookExecutorImpl + lifecycle fires) ─
//
// The mobile composition root now builds the REAL `HookExecutorImpl` (loaded
// from `cwd/.lingxi/settings.json` + `lingxi_home/settings.json`) in place of
// the `noop_hook_executor()` stub, and fires `SessionStart` (source=startup)
// + `InstructionsLoaded` (once per loaded LINGXI.md) at boot — exactly the
// desktop `build()` lifecycle (harness-runtime::desktop §7 / §7.1). These mirror the
// desktop `build_fires_session_start_against_a_registered_hook` /
// `build_fires_instructions_loaded_against_a_registered_hook` /
// `build_with_injected_memory_reaches_system_prompt` tests.

/// Write a single command hook for `event` into the project settings the
/// mobile hook loader reads at boot (`<cwd>/.lingxi/settings.json`). The
/// `"true"` command is a side-effect-free no-op (the in-build lifecycle fire
/// is best-effort), so this asserts hook *registration*, not the command's
/// effect.
fn write_project_hook(cwd: &std::path::Path, event: &str) {
    let lingxi_dir = cwd.join(".lingxi");
    std::fs::create_dir_all(&lingxi_dir).expect("mk .lingxi");
    std::fs::write(
        lingxi_dir.join("settings.json"),
        format!(
            r#"{{ "hooks": {{ "{event}": [ {{ "hooks": [
                {{ "type": "command", "command": "true" }}
            ] }} ] }} }}"#
        ),
    )
    .expect("write settings.json");
}

/// P0.2: the boot path fires `SessionStart` (source=startup) once the
/// orchestrator + hook registry are wired, best-effort. We register a
/// `SessionStart` command hook in the project settings; `build_mobile` must
/// (a) succeed even though the wired `fire_session_start("startup")` ran a
/// (no-op) command hook, and (b) surface the loaded hook via `list_hooks` —
/// proving the boot path loaded the session-lifecycle hook the in-build fire
/// dispatched against (mobile sibling of the desktop test).
#[tokio::test]
async fn build_mobile_fires_session_start_against_a_registered_hook() {
    use lingxi_core::host::OrchestratorHandle as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    write_project_hook(tmp.path(), "SessionStart");

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());

    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile must succeed even with a (no-op) SessionStart hook registered");

    let hooks = rt.orchestrator.list_hooks().await;
    // Exactly ONE registration. On mobile `lingxi_home` == `<cwd>/.claude`, so
    // the user- and project-settings paths resolve to the SAME file; before the
    // settings-path de-dup this hook registered (and therefore fired) TWICE. A
    // plain `.any()` masked that — assert count == 1 to catch the regression.
    let session_start_count = hooks.iter().filter(|h| h.event == "SessionStart").count();
    assert_eq!(
        session_start_count, 1,
        "boot must load the SessionStart hook EXACTLY once (no settings-path double-registration): {hooks:?}"
    );
}

/// v3 Phase 1: a workflow runs END TO END on mobile — launched through the
/// same `MobileWorkflowLauncher` the registered Workflow tool holds, the
/// QuickJS runtime executes the script on its own thread, the task
/// reaches a terminal status, the spool captures the phase/log output,
/// and the completion drains exactly once through the task-notification
/// path the orchestrator's per-turn reminder reads. The script makes no
/// `agent()` calls, so this exercises registry + launcher + runtime +
/// status sink without an LLM.
#[tokio::test]
async fn workflow_launches_and_completes_on_mobile() {
    use tool_workflow::WorkflowLauncher as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let listener_for_build: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let rt = build_mobile(
        test_config(tmp.path()),
        platform,
        listener_for_build,
        perm_sink,
    )
    .await
    .expect("build_mobile");

    let launcher = crate::mobile::workflow_support::MobileWorkflowLauncher {
        registry: rt.task_registry.clone(),
        project_cwd: tmp.path().to_path_buf(),
        current_cwd: Arc::new(std::sync::Mutex::new(tmp.path().to_path_buf())),
        lingxi_home: tmp.path().join(".claude"),
        // The launcher and status sink must share the engine's live
        // session watermark; a detached fixture uuid would correctly
        // suppress the completion event as stale.
        session_uuid: rt.active_session_uuid.clone(),
        default_model_selection_provider: Arc::new(std::sync::OnceLock::new()),
        checkpoints: rt.workflow_checkpoints.clone(),
        status_sink: rt.workflow_status_sink.clone(),
    };
    let launched = launcher
        .launch(tool_workflow::WorkflowLaunchSpec {
            script: Some(
                "export const meta = { name: 'phase1-smoke', description: 'p1 smoke' }\n\
                     phase('Only')\n\
                     log('hello from quickjs')\n\
                     return 41 + 1\n"
                    .into(),
            ),
            name: None,
            script_path: None,
            args: None,
            resume_from_run_id: None,
            session_uuid: None,
            ..Default::default()
        })
        .await
        .expect("launch succeeds");
    assert!(
        launched
            .run_id
            .as_deref()
            .is_some_and(|r| r.starts_with("wf_")),
        "{launched:?}"
    );
    let transcript_dir = launched
        .transcript_dir
        .as_deref()
        .expect("workflow launch returns its transcript directory");
    assert!(
        std::path::Path::new(transcript_dir).is_dir(),
        "launcher must create the transcript directory before a child can append JSONL"
    );

    // Poll to a terminal status (the script thread is fast; bound the wait).
    let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle = &*rt.task_registry;
    let mut status = String::new();
    for _ in 0..100 {
        let record = registry
            .get(&launched.task_id)
            .await
            .expect("get")
            .expect("task exists");
        status = record.status.clone();
        if status != "pending" && status != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(status, "completed", "the QuickJS run must complete");
    assert!(
        listener.received.lock().await.iter().any(|event| matches!(
            event,
            ClientEvent::TaskStatusChanged { task_id, status, .. }
                if task_id == &launched.task_id
                    && *status == client::protocol::listings::TaskStatusDto::Completed
        )),
        "workflow completion must be pushed to the mobile client"
    );

    // The spool captured the phase header + the log line.
    let chunk = registry
        .output(&launched.task_id, None)
        .await
        .expect("output");
    assert!(
        chunk.content.contains("hello from quickjs"),
        "spool must carry log() output: {}",
        chunk.content
    );

    // Completion surfaces exactly once through the notification drain the
    // orchestrator's `<task-notification>` reminder consumes.
    let notes = rt
        .task_registry
        .take_pending_task_notifications(
            lingxi_core::host::task_delivery::TaskDeliveryPolicy::default(),
        )
        .await;
    assert!(
        notes.iter().any(|n| n.task_id == launched.task_id),
        "completed workflow must be drained as a task notification: {notes:?}"
    );
    let again = rt
        .task_registry
        .take_pending_task_notifications(
            lingxi_core::host::task_delivery::TaskDeliveryPolicy::default(),
        )
        .await;
    assert!(
        !again.iter().any(|n| n.task_id == launched.task_id),
        "consume-once: a second drain must not re-surface it"
    );
}

#[tokio::test]
async fn workflow_global_fusion_is_not_exposed_on_mobile() {
    use tool_workflow::WorkflowLauncher as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let listener_for_build: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let rt = build_mobile(
        test_config(tmp.path()),
        platform,
        listener_for_build,
        perm_sink,
    )
    .await
    .expect("build_mobile");

    let launched = rt
            .workflow_launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                script: Some(
                    "export const meta = { name: 'fusion-mobile', description: 'mobile fusion gate' }\n\
                     return await fusion('review this')\n"
                        .into(),
                ),
                ..Default::default()
            })
            .await
            .expect("launch succeeds");

    let registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle = &*rt.task_registry;
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let record = registry
                .get(&launched.task_id)
                .await
                .expect("get")
                .expect("task exists");
            if record.status != "pending" && record.status != "running" {
                break record.status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("mobile fusion task did not reach a terminal status");
    assert_eq!(status, "failed");

    let chunk = registry
        .output(&launched.task_id, None)
        .await
        .expect("output");
    assert!(
        chunk
            .content
            .contains("ReferenceError: fusion is not defined"),
        "the removed workflow fusion global must remain unavailable: {}",
        chunk.content
    );
}

#[tokio::test]
async fn workflow_relative_script_path_uses_live_cwd_but_session_files_stay_under_project_root() {
    use tool_workflow::WorkflowLauncher as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let listener_for_build: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let rt = build_mobile(
        test_config(tmp.path()),
        platform,
        listener_for_build,
        perm_sink,
    )
    .await
    .expect("build_mobile");

    let live_dir = tmp.path().join("live");
    std::fs::create_dir_all(&live_dir).expect("create live dir");
    std::fs::write(
        live_dir.join("workflow.js"),
        "export const meta = { name: 'live-cwd', description: 'relative path' }\nreturn 1\n",
    )
    .expect("write workflow");
    let current_cwd = Arc::new(std::sync::Mutex::new(live_dir.clone()));
    let launcher = crate::mobile::workflow_support::MobileWorkflowLauncher {
        registry: rt.task_registry.clone(),
        project_cwd: tmp.path().to_path_buf(),
        current_cwd,
        lingxi_home: tmp.path().join(".claude"),
        session_uuid: rt.active_session_uuid.clone(),
        default_model_selection_provider: Arc::new(std::sync::OnceLock::new()),
        checkpoints: rt.workflow_checkpoints.clone(),
        status_sink: rt.workflow_status_sink.clone(),
    };

    let launched = launcher
        .launch(tool_workflow::WorkflowLaunchSpec {
            script: None,
            name: None,
            script_path: Some("workflow.js".into()),
            script_path_approval: Some(tool_workflow::WorkflowScriptPathApproval {
                requested: live_dir.join("workflow.js"),
                approved: std::fs::canonicalize(live_dir.join("workflow.js"))
                    .expect("canonical workflow"),
                root: std::fs::canonicalize(&live_dir).expect("canonical live dir"),
                relative: "workflow.js".into(),
            }),
            args: None,
            resume_from_run_id: None,
            session_uuid: None,
            tool_use_id: None,
            launched_from_subagent: false,
            ..Default::default()
        })
        .await
        .expect("launch succeeds");

    assert_eq!(
        launched.script_path.as_deref(),
        Some(live_dir.join("workflow.js").to_string_lossy().as_ref())
    );
    let transcript_dir = launched
        .transcript_dir
        .as_deref()
        .expect("workflow launch returns its transcript directory")
        .to_string();
    assert!(
        transcript_dir.starts_with(tmp.path().join(".claude").to_string_lossy().as_ref()),
        "transcript dir must stay anchored at the project session root: {transcript_dir}"
    );
}

/// P0.2: the boot path fires `InstructionsLoaded` (load_reason=session_start)
/// right after `SessionStart`, best-effort. We register an
/// `InstructionsLoaded` command hook in the project settings; `build_mobile`
/// must succeed and surface the loaded hook via `list_hooks`. (With the
/// default empty memory provider no instruction file actually fires, exactly
/// like the desktop sibling — the assertion pins the boot-fire seam + the
/// real registry wiring.)
#[tokio::test]
async fn build_mobile_fires_instructions_loaded_against_a_registered_hook() {
    use lingxi_core::host::OrchestratorHandle as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    write_project_hook(tmp.path(), "InstructionsLoaded");

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());

    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile must succeed even with a (no-op) InstructionsLoaded hook");

    let hooks = rt.orchestrator.list_hooks().await;
    assert!(
        hooks.iter().any(|h| h.event == "InstructionsLoaded"),
        "boot must load the InstructionsLoaded hook the lifecycle fire dispatches against: {hooks:?}"
    );
}

/// P0.2: the injectable `cfg.memory_provider` seam (production wires
/// `orchestrator::prompt::real_provider()`). We inject a CONTROLLED in-memory
/// provider (NOT the real FS) carrying one project LINGXI.md and prove it
/// flows through `build_mobile` into the orchestrator's system prompt (the
/// GAP-3 memory section — preamble + tier-tagged `Contents of …:` + body).
/// The default-empty sibling elides the memory section, so its presence is
/// the load-bearing difference the injected provider makes.
#[tokio::test]
async fn build_mobile_with_injected_memory_reaches_system_prompt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = test_config(tmp.path());

    let memory_path = cfg.cwd.join("LINGXI.md");
    let memory_body = "PROJECT MEMORY: always be terse.";
    let memory_file = orchestrator::prompt::MemoryFile {
        parent: None,
        source_content: None,
        path: memory_path.clone(),
        body: memory_body.to_string(),
        is_local_override: false,
        tier: orchestrator::prompt::LingxiMdTier::Project,
        globs: None,
        raw_content: memory_body.to_string(),
        content_differs_from_disk: false,
    };
    cfg.memory_provider = Some(Arc::new(
        orchestrator::test_support::StaticMemoryProvider::with_files(vec![memory_file]),
    ));

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());

    let rt = build_mobile(cfg, platform, listener, perm_sink)
        .await
        .expect("build_mobile with an injected memory provider must succeed");

    // R-P1: instructions lives in the leading additional-context `<system-reminder>`
    // meta now (same `memory_block::format`), NOT the system prompt.
    let ctx = rt
        .orchestrator
        .additional_context_preview()
        .await
        .expect("additional-context preview must succeed")
        .expect("an additional-context meta must be present (currentDate is unconditional)");
    assert!(
            ctx.contains(
                "Codebase and user instructions are shown below. Be sure to adhere to these instructions."
            ),
            "injected memory must emit the memory preamble in the additional-context meta: {ctx}"
        );
    assert!(
        ctx.contains(&format!(
            "Contents of {} (project instructions, checked into the codebase):",
            memory_path.display()
        )),
        "the injected LINGXI.md must emit a tier-tagged `Contents of …:` marker: {ctx}"
    );
    assert!(
        ctx.contains(memory_body),
        "the injected LINGXI.md body must appear in the additional-context meta: {ctx}"
    );
}

/// P0.2 determinism guard: a default-config `build_mobile`
/// (`cfg.memory_provider == None`) loads NO memory, so the system prompt
/// emits NO memory section — pinning that the existing boot tests stay
/// deterministic (they never read the real `~/.lingxi/LINGXI.md`).
#[tokio::test]
async fn build_mobile_default_loads_no_memory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(tmp.path());
    assert!(
        cfg.memory_provider.is_none(),
        "default config must leave memory_provider None (empty, deterministic)"
    );

    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());

    let rt = build_mobile(cfg, platform, listener, perm_sink)
        .await
        .expect("default build_mobile must succeed");

    let sys = rt
        .orchestrator
        .assemble_system_prompt_preview()
        .await
        .expect("system prompt preview must succeed");
    assert!(
            !sys.contains(
                "Codebase and user instructions are shown below. Be sure to adhere to these instructions."
            ),
            "a default build must emit NO memory preamble: {sys}"
        );
}

// ── F3-05: the async `submit` FFI entry point ───────────────────────────

use super::{build_mobile_engine, MobileEngineHandle};
use client::protocol::commands::ClientCommand;
use client::protocol::error::ClientError;
use client::protocol::events::{ClientEvent as Ev, TurnRecoveryStateDto};
use client::protocol::permission::PermissionResponseDto;
use lingxi_core::host::audio::{
    AudioCapabilitySnapshot, AudioError, AudioOperation, AudioOperationContext, AudioOperationId,
    AudioOperationKind, AudioOperationSuccess, AudioOwner, AudioService,
};

/// Build a real, fully-wired [`MobileEngineHandle`] off-device (host fake
/// `Platform`) so the F3-05 `submit` path is exercised on CI. Returns the
/// handle plus the recording listener so a test can read back delivered
/// events.
fn build_submit_handle(root: &std::path::Path) -> (Arc<MobileEngineHandle>, Arc<FakeListener>) {
    build_submit_handle_with_platform(root, Arc::new(HostFakePlatform::new(root.to_path_buf())))
}

fn build_submit_handle_with_platform(
    root: &std::path::Path,
    platform: Arc<dyn lingxi_core::host::Platform>,
) -> (Arc<MobileEngineHandle>, Arc<FakeListener>) {
    let listener = Arc::new(FakeListener::default());
    let listener_dyn: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let handle = build_mobile_engine(test_config(root), platform, listener_dyn, perm_sink)
        .expect("build_mobile_engine failed");
    (handle, listener)
}

#[test]
fn foreign_small_stack_can_construct_engine() {
    let root = tempfile::tempdir().expect("foreign constructor root");
    std::thread::Builder::new()
        .name("foreign-swift-build".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            let (_handle, _listener) = build_submit_handle(root.path());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn foreign_small_stack_can_refresh_slash_command_catalog() {
    let root = tempfile::tempdir().expect("foreign caller root");
    let (handle, listener) = build_submit_handle(root.path());
    std::thread::Builder::new()
        .name("foreign-swift-poll".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            futures::executor::block_on(handle.submit(ClientCommand::RefreshListings {
                which: vec![client::protocol::commands::ListingKindDto::SlashCommands],
            }))
            .expect("foreign caller can refresh the catalog");
            assert!(listener
                .received
                .blocking_lock()
                .iter()
                .any(|event| { matches!(event, Ev::SlashCommandCatalog { .. }) }));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn mobile_controls_event_contains_the_live_native_proactivity_snapshot() {
    let root = tempfile::tempdir().expect("mobile controls root");
    let (handle, listener) = build_submit_handle(root.path());
    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::InitializeProactivity {
                proactivity: Some(true),
                reinitialize: Some(false),
            })
            .await
            .expect("Mobile routes the Native session opt-in command");
        let expected = client::adapter::controls::lower_native_proactivity_control_state(
            lingxi_core::host::OrchestratorHandle::native_proactivity_snapshot(
                handle.inner.orchestrator.as_ref(),
            ),
        );
        assert!(
            expected.is_some(),
            "the production Mobile composition exposes the current opt-in control"
        );

        handle.emit_controls_snapshot().await;
        let events = listener.received.lock().await;
        let controls = events
            .iter()
            .rev()
            .find_map(|event| match event {
                Ev::ConversationControlsChanged { controls } => Some(controls),
                _ => None,
            })
            .expect("Mobile emits a controls snapshot");
        assert_eq!(controls.proactivity, expected);
    });
}

struct AudioHostFakePlatform {
    base: HostFakePlatform,
    audio: Arc<dyn AudioService>,
}

impl lingxi_core::host::Platform for AudioHostFakePlatform {
    fn filesystem(&self) -> Arc<dyn lingxi_core::host::FileSystem> {
        self.base.filesystem()
    }

    fn http(&self) -> Arc<dyn lingxi_core::host::HttpTransport> {
        self.base.http()
    }

    fn clock(&self) -> Arc<dyn lingxi_core::host::Clock> {
        self.base.clock()
    }

    fn process(&self) -> Arc<dyn lingxi_core::host::ProcessRunner> {
        self.base.process()
    }

    fn sandbox(&self) -> Arc<dyn lingxi_core::host::Sandbox> {
        self.base.sandbox()
    }

    fn worktree(&self) -> Arc<dyn lingxi_core::host::WorktreeManager> {
        self.base.worktree()
    }

    fn audio_service(&self) -> Option<Arc<dyn AudioService>> {
        Some(self.audio.clone())
    }
}

#[derive(Default)]
struct AudioOwnerTeardownProbe {
    operations: StdMutex<Vec<(AudioOwner, AudioOperation)>>,
    operations_changed: std::sync::Condvar,
}

#[async_trait]
impl AudioService for AudioOwnerTeardownProbe {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        AudioCapabilitySnapshot {
            service_epoch: 19,
            support_revision: 1,
            supported_operations: vec![AudioOperationKind::Record],
            readiness: Vec::new(),
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.operations
            .lock()
            .unwrap()
            .push((context.owner, operation.clone()));
        self.operations_changed.notify_all();
        match operation {
            AudioOperation::EndOwner => Ok(AudioOperationSuccess::OwnerEnded),
            _ => Err(AudioError::new(
                lingxi_core::host::audio::AudioErrorKind::Unsupported,
                "teardown probe only accepts EndOwner",
            )),
        }
    }

    async fn cancel(&self, _identity: AudioOperationId) -> Result<(), AudioError> {
        Ok(())
    }
}

#[derive(Default)]
struct AudioDropGateState {
    callback_started: bool,
    drop_returned: bool,
    callback_completed: bool,
}

struct AudioDropGateService {
    state: Arc<(StdMutex<AudioDropGateState>, std::sync::Condvar)>,
}

#[async_trait]
impl AudioService for AudioDropGateService {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        AudioCapabilitySnapshot {
            service_epoch: 20,
            support_revision: 1,
            supported_operations: vec![AudioOperationKind::Record],
            readiness: Vec::new(),
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        _context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        assert!(matches!(operation, AudioOperation::EndOwner));
        let (state, changed) = self.state.as_ref();
        let mut state = state.lock().unwrap();
        state.callback_started = true;
        changed.notify_all();
        while !state.drop_returned {
            state = changed.wait(state).unwrap();
        }
        state.callback_completed = true;
        changed.notify_all();
        Ok(AudioOperationSuccess::OwnerEnded)
    }

    async fn cancel(&self, _identity: AudioOperationId) -> Result<(), AudioError> {
        Ok(())
    }
}

#[test]
fn typescript_lsp_mode_defaults_to_auto_and_preserves_unrelated_settings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let settings = tmp.path().join("settings.json");
    assert_eq!(
        super::mobile_typescript_lsp_mode(&settings).unwrap(),
        lsp::LspActivationMode::Auto
    );
    std::fs::write(
        &settings,
        r#"{"theme":"dark","lsp":{"other":{"enabled":true}}}"#,
    )
    .unwrap();
    super::persist_mobile_typescript_lsp_mode(&settings, lsp::LspActivationMode::On)
        .expect("persist global LSP setting");
    assert_eq!(
        super::mobile_typescript_lsp_mode(&settings).unwrap(),
        lsp::LspActivationMode::On
    );
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(root["theme"], "dark");
    assert_eq!(root["lsp"]["other"]["enabled"], true);
    assert_eq!(root["lsp"]["typescript"]["mode"], "on");

    std::fs::write(&settings, r#"{"lsp":{"typescript":{"mode":"maybe"}}}"#).unwrap();
    assert!(super::mobile_typescript_lsp_mode(&settings).is_err());
}

#[test]
fn typescript_lsp_command_persists_requested_and_reports_degraded_effective_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SetTypescriptLspMode {
                mode: "on".to_string(),
            })
            .await
            .expect("valid mode should persist");
        let events = listener.received.lock().await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::TypescriptLspModeChanged { requested, effective, available }
                if requested == "on" && effective == "off" && !available
        )));
    });
    let settings = tmp.path().join(".lingxi/settings.json");
    assert_eq!(
        super::mobile_typescript_lsp_mode(&settings).unwrap(),
        lsp::LspActivationMode::On
    );
}

/// `build_submit_handle` with a caller-supplied config, for tests that need
/// a specific routing allowlist or default model.
fn build_submit_handle_with_config(
    cfg: MobileConfig,
    root: &std::path::Path,
) -> (Arc<MobileEngineHandle>, Arc<FakeListener>) {
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(root.to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let listener_dyn: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let handle = build_mobile_engine(cfg, platform, listener_dyn, perm_sink)
        .expect("build_mobile_engine failed");
    (handle, listener)
}

fn build_submit_handle_with_secure_store(
    root: &std::path::Path,
) -> (Arc<MobileEngineHandle>, Arc<FakeListener>) {
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(root.to_path_buf())
            .with_secure_storage(Arc::new(FakeEncryptedStore::default())),
    );
    let listener = Arc::new(FakeListener::default());
    let listener_dyn: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let handle = build_mobile_engine(test_config(root), platform, listener_dyn, perm_sink)
        .expect("build_mobile_engine failed");
    (handle, listener)
}

#[test]
fn mobile_engine_exports_copyable_mcp_oauth_url_getters() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());
    assert_eq!(handle.mcp_oauth_authorization_url(), None);
    assert_eq!(handle.mobile_mcp_oauth_authorization_url(), None);
}

#[test]
fn mobile_mcp_reload_identical_config_is_read_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(tmp.path());
    let settings_path = cfg.lingxi_home.join("settings.json");
    std::fs::create_dir_all(&cfg.lingxi_home).expect("create mobile home");
    std::fs::write(
        &settings_path,
        r#"{"mcpServers":{"remote":{"type":"http","url":"http://127.0.0.1:1/mcp"}}}"#,
    )
    .expect("write MCP settings");
    let cwd = cfg.cwd.clone();
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());
    let desired = mobile_mcp_preflight(
        mcp::load_mcp_servers(&cwd.join(".mcp.json"), &settings_path, &cwd),
        handle.inner.oauth_supported,
    )
    .into_iter()
    .find(|config| config.name == "remote")
    .expect("settings MCP entry");
    let connection_id = lingxi_core::types::McpConnectionId::new();
    handle.runtime().block_on(async {
        handle.inner.mcp_registry.connections.write().await.insert(
            "remote".into(),
            mcp::connection::McpConnectionState::Connected {
                config: desired.clone(),
                connection_id,
                capabilities: lingxi_core::host::ServerCapabilitiesDto::default(),
                negotiated: lingxi_core::host::McpNegotiatedProtocol {
                    era: lingxi_core::host::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: Vec::new(),
                connected_at: std::time::SystemTime::now(),
            },
        );
        handle.reload_configured_mcp().await;
        let state = handle
            .inner
            .mcp_registry
            .connections
            .read()
            .await
            .get("remote")
            .cloned()
            .expect("unchanged MCP entry remains installed");
        match state {
            mcp::connection::McpConnectionState::Connected {
                connection_id: actual_id,
                config,
                ..
            } => {
                assert_eq!(actual_id, connection_id);
                assert_eq!(
                    super::mobile_mcp_config_snapshot(&config),
                    super::mobile_mcp_config_snapshot(&desired)
                );
            }
            other => panic!("unchanged MCP entry was reconciled: {other:?}"),
        }
    });
}

#[test]
fn mobile_mcp_reload_retains_plugin_scoped_servers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(tmp.path());
    std::fs::create_dir_all(&cfg.lingxi_home).expect("create mobile home");
    std::fs::write(
        cfg.lingxi_home.join("settings.json"),
        r#"{"mcpServers":{}}"#,
    )
    .expect("write empty MCP settings");
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());
    let plugin_name = "mobile-fixture";
    let plugin_dir = tmp.path().join("plugins");
    std::fs::create_dir_all(plugin_dir.join(plugin_name).join(".lingxi-plugin"))
        .expect("plugin manifest dir");
    std::fs::write(
            plugin_dir
                .join(plugin_name)
                .join(".lingxi-plugin")
                .join("plugin.json"),
            r#"{"name":"mobile-fixture","version":"1.0.0","mcpServers":{"srv":{"type":"stdio","command":"echo"}}}"#,
        )
        .expect("write plugin manifest");
    let discovered = handle
        .runtime()
        .block_on(async { plugin::discovery::discover_installed_plugins(&plugin_dir).await });
    let (id, manifest, install_dir) = discovered
        .into_iter()
        .find(|(_, manifest, _)| manifest.name == plugin_name)
        .expect("fixture plugin discovered");
    handle.runtime().block_on(async {
        handle
            .inner
            .wired_plugin_manager
            .enable(&id, manifest, install_dir)
            .await
            .expect("enable fixture plugin");
        assert!(
            handle
                .inner
                .mcp_registry
                .connections
                .read()
                .await
                .contains_key("plugin:mobile-fixture:srv"),
            "fixture plugin must materialize an MCP server before reload"
        );
        handle.reload_configured_mcp().await;
        assert!(
            handle
                .inner
                .mcp_registry
                .connections
                .read()
                .await
                .contains_key("plugin:mobile-fixture:srv"),
            "settings reload must retain plugin-scoped MCP servers"
        );
    });
}

fn mobile_reload_test_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        name: "remote".into(),
        spec: lingxi_core::host::McpTransportSpec::Http {
            url: url.into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: None,
        },
        scope: McpConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        discovery_cache: None,
        config_error: None,
        metadata: Default::default(),
    }
}

struct BlockingMobileMcpTransport {
    connect_started: Notify,
    connect_release: Notify,
    block_connect: AtomicBool,
    connect_calls: AtomicUsize,
    disconnect_calls: AtomicUsize,
}

impl BlockingMobileMcpTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            connect_started: Notify::new(),
            connect_release: Notify::new(),
            block_connect: AtomicBool::new(false),
            connect_calls: AtomicUsize::new(0),
            disconnect_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl lingxi_core::host::McpTransport for BlockingMobileMcpTransport {
    async fn connect(
        &self,
        _spec: &lingxi_core::host::McpTransportSpec,
    ) -> Result<lingxi_core::host::McpRawConnection, lingxi_core::host::McpError> {
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        self.connect_started.notify_one();
        if self.block_connect.load(Ordering::SeqCst) {
            self.connect_release.notified().await;
        }
        Ok(lingxi_core::host::McpRawConnection {
            connection_id: lingxi_core::types::McpConnectionId::new(),
        })
    }

    async fn initialize(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
    ) -> Result<lingxi_core::host::ServerCapabilitiesDto, lingxi_core::host::McpError> {
        Ok(lingxi_core::host::ServerCapabilitiesDto::default())
    }

    async fn list_tools(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpToolDto>, lingxi_core::host::McpError> {
        Ok(Vec::new())
    }

    async fn list_resources(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpResourceDto>, lingxi_core::host::McpError> {
        Ok(Vec::new())
    }

    async fn list_prompts(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpPromptDto>, lingxi_core::host::McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
        _tool: &str,
        _input: Utf16JsonProjection,
    ) -> Result<lingxi_core::host::McpToolResultDto, lingxi_core::host::McpError> {
        Err(lingxi_core::host::McpError::Internal(
            "unused test tool call".into(),
        ))
    }

    async fn read_resource(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
        _uri: &Utf16JsonProjection,
    ) -> Result<lingxi_core::host::McpResourceContentDto, lingxi_core::host::McpError> {
        Err(lingxi_core::host::McpError::Internal(
            "unused test resource read".into(),
        ))
    }

    async fn ping(
        &self,
        _connection_id: lingxi_core::types::McpConnectionId,
    ) -> Result<(), lingxi_core::host::McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
    ) -> Result<lingxi_core::host::McpNotificationStream, lingxi_core::host::McpError> {
        Ok(Box::pin(futures_util::stream::empty()))
    }

    async fn handle_elicitation(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
        _request: lingxi_core::host::ElicitRequestDto,
    ) -> Result<lingxi_core::host::ElicitResultDto, lingxi_core::host::McpError> {
        Err(lingxi_core::host::McpError::Internal(
            "unused test elicitation".into(),
        ))
    }

    async fn disconnect(
        &self,
        _connection_id: lingxi_core::types::McpConnectionId,
    ) -> Result<(), lingxi_core::host::McpError> {
        self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn supported_transports(&self) -> Vec<lingxi_core::host::McpTransportKind> {
        vec![lingxi_core::host::McpTransportKind::Http]
    }
}

#[test]
fn mobile_mcp_startup_blocked_a_is_superseded_by_b() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let transport = BlockingMobileMcpTransport::new();
        transport.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::new(
            transport.clone() as Arc<dyn lingxi_core::host::McpTransport>
        ));
        let generations = Arc::new(StdMutex::new(HashMap::new()));
        let config_a = mobile_reload_test_config("http://127.0.0.1:1/startup-a");
        let config_b = mobile_reload_test_config("http://127.0.0.1:1/startup-b");
        let (generation_a, _) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        let startup = tokio::spawn(mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_a,
                desired: config_a,
                previous: None,
            },
            registry.clone(),
            generations.clone(),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            transport.connect_started.notified(),
        )
        .await
        .expect("startup A reaches the blocked transport");
        let (generation_b, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_b));
        assert!(changed);
        transport.block_connect.store(false, Ordering::SeqCst);
        transport.connect_release.notify_one();
        startup.await.expect("startup A job");
        assert!(registry.connections.read().await.is_empty());

        mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_b,
                desired: config_b.clone(),
                previous: None,
            },
            registry.clone(),
            generations,
        )
        .await;
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Connected { config, .. })
                if mobile_mcp_config_unchanged(config, &config_b)
        ));
        assert_eq!(transport.connect_calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn submit_attach_turn_emits_snapshot_then_replays_strictly_after_cursor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                77,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin durable turn");
        handle
            .durable_turns
            .append_event(
                &session_id,
                77,
                r#"{"type":"text_delta","text":"one"}"#.to_string(),
            )
            .expect("first event");
        handle
            .durable_turns
            .append_event(
                &session_id,
                77,
                r#"{"type":"text_delta","text":"two"}"#.to_string(),
            )
            .expect("second event");
        listener.received.lock().await.clear();

        handle
            .submit(ClientCommand::AttachTurn {
                turn_id: 77,
                after_sequence: Some(1),
            })
            .await
            .expect("attach turn");

        let events = listener.received.lock().await.clone();
        assert!(matches!(
            events.first(),
            Some(Ev::TurnRecoveryState { snapshot })
                if snapshot.session_id == session_id
                    && snapshot.turn_id == 77
                    && snapshot.last_sequence == 2
        ));
        assert!(matches!(
            events.get(1),
            Some(Ev::TurnEventReplay {
                session_id: replay_session,
                turn_id: 77,
                sequence: 2,
                event_json,
            }) if replay_session == &session_id && event_json.contains("two")
        ));
        assert_eq!(
            events.len(),
            2,
            "cursor must suppress the first retained event"
        );
    });
}

#[test]
fn mobile_mcp_repeated_identical_reload_keeps_blocked_startup_owner() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let transport = BlockingMobileMcpTransport::new();
        transport.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::new(
            transport.clone() as Arc<dyn lingxi_core::host::McpTransport>
        ));
        let generations = Arc::new(StdMutex::new(HashMap::new()));
        let config = mobile_reload_test_config("http://127.0.0.1:1/repeated");
        let (generation, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config));
        assert!(changed);
        let startup = tokio::spawn(mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation,
                desired: config.clone(),
                previous: None,
            },
            registry.clone(),
            generations.clone(),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            transport.connect_started.notified(),
        )
        .await
        .expect("startup reaches blocked transport");

        let current = registry
            .connections
            .read()
            .await
            .get("remote")
            .cloned()
            .expect("blocked startup owns a transitional state");
        assert!(mobile_mcp_state_is_transitional(&current));
        let (repeat_generation, intent_changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config));
        assert_eq!(repeat_generation, generation);
        assert!(!intent_changed);
        assert!(
            !mobile_mcp_reload_requires_replacement(&current, &config, intent_changed),
            "an identical reload must retain the blocked startup owner"
        );
        assert_eq!(transport.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.disconnect_calls.load(Ordering::SeqCst), 0);

        transport.block_connect.store(false, Ordering::SeqCst);
        transport.connect_release.notify_one();
        startup.await.expect("startup owner completes");
        assert_eq!(transport.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.disconnect_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Connected { config: current, .. })
                if mobile_mcp_config_unchanged(current, &config)
        ));
        let connected = registry
            .connections
            .read()
            .await
            .get("remote")
            .cloned()
            .expect("startup owner remains connected");
        assert!(
            !mobile_mcp_reload_requires_replacement(&connected, &config, false),
            "a settled identical reload must remain read-only"
        );
        let changed = mobile_reload_test_config("http://127.0.0.1:1/replacement");
        assert!(
            mobile_mcp_reload_requires_replacement(&connected, &changed, false),
            "a settled changed config must schedule replacement"
        );
    });
}

#[test]
fn submit_attach_turn_blocks_truncated_replay_history() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                78,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("hello"),
                None,
                Vec::new(),
            )
            .expect("begin durable turn");
        for index in 0..2 {
            handle
                .durable_turns
                .append_event(
                    &session_id,
                    78,
                    format!(r#"{{"type":"text_delta","text":"{index}"}}"#),
                )
                .expect("append event");
        }
        handle
            .durable_turns
            .retain_events_from_for_test(&session_id, 78, 2)
            .expect("truncate replay prefix");
        listener.received.lock().await.clear();

        handle
            .submit(ClientCommand::AttachTurn {
                turn_id: 78,
                after_sequence: None,
            })
            .await
            .expect("attach turn");

        let events = listener.received.lock().await.clone();
        assert!(matches!(
            events.as_slice(),
            [Ev::TurnRecoveryState { snapshot }]
                if snapshot.session_id == session_id
                    && snapshot.turn_id == 78
                    && snapshot.state == TurnRecoveryStateDto::WaitingForUser
                    && !snapshot.safe_to_resume
                    && snapshot.reason.as_deref() == Some("replay_history_truncated")
        ));
    });
}

#[test]
fn mobile_mcp_blocked_a_to_b_to_a_keeps_latest_a_owner() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let transport = BlockingMobileMcpTransport::new();
        transport.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::new(
            transport.clone() as Arc<dyn lingxi_core::host::McpTransport>
        ));
        let generations = Arc::new(StdMutex::new(HashMap::new()));
        let config_a = mobile_reload_test_config("http://127.0.0.1:1/a");
        let config_b = mobile_reload_test_config("http://127.0.0.1:1/b");
        let (generation_a, _) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        let stale = tokio::spawn(mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_a,
                desired: config_a.clone(),
                previous: None,
            },
            registry.clone(),
            generations.clone(),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            transport.connect_started.notified(),
        )
        .await
        .expect("A reaches blocked connect");
        let (generation_b, _) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_b));
        let (generation_latest_a, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        assert!(changed);
        assert!(generation_latest_a > generation_b);
        transport.block_connect.store(false, Ordering::SeqCst);
        transport.connect_release.notify_one();
        stale.await.expect("stale A job");
        assert!(registry.connections.read().await.is_empty());

        mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_latest_a,
                desired: config_a.clone(),
                previous: None,
            },
            registry.clone(),
            generations,
        )
        .await;
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Connected { config, .. })
                if mobile_mcp_config_unchanged(config, &config_a)
        ));
        assert_eq!(transport.connect_calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn mobile_mcp_blocked_a_to_deleted_to_a_keeps_latest_a_owner() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let transport = BlockingMobileMcpTransport::new();
        transport.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::new(
            transport.clone() as Arc<dyn lingxi_core::host::McpTransport>
        ));
        let generations = Arc::new(StdMutex::new(HashMap::new()));
        let config_a = mobile_reload_test_config("http://127.0.0.1:1/a");
        let (generation_a, _) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        let stale = tokio::spawn(mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_a,
                desired: config_a.clone(),
                previous: None,
            },
            registry.clone(),
            generations.clone(),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            transport.connect_started.notified(),
        )
        .await
        .expect("A reaches blocked connect");
        let (generation_deleted, _) = mobile_mcp_record_reload_intent(&generations, "remote", None);
        let (generation_latest_a, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        assert!(changed);
        assert!(generation_latest_a > generation_deleted);
        transport.block_connect.store(false, Ordering::SeqCst);
        transport.connect_release.notify_one();
        stale.await.expect("stale A job");
        assert!(registry.connections.read().await.is_empty());

        mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_latest_a,
                desired: config_a.clone(),
                previous: None,
            },
            registry.clone(),
            generations,
        )
        .await;
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Connected { config, .. })
                if mobile_mcp_config_unchanged(config, &config_a)
        ));
        assert_eq!(transport.connect_calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn mobile_mcp_changed_to_disabled_reloads_without_dialing() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let transport = BlockingMobileMcpTransport::new();
        let registry = Arc::new(McpRegistry::new(
            transport.clone() as Arc<dyn lingxi_core::host::McpTransport>
        ));
        let config_a = mobile_reload_test_config("http://127.0.0.1:1/a");
        let mut disabled = config_a.clone();
        disabled.disabled = true;
        registry.connections.write().await.insert(
            "remote".into(),
            mcp::connection::McpConnectionState::Disconnected {
                config: config_a.clone(),
                last_error: None,
            },
        );
        let generations = Arc::new(StdMutex::new(HashMap::new()));
        let (generation_a, _) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        let (generation_disabled, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&disabled));
        assert!(changed);
        mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_disabled,
                desired: disabled.clone(),
                previous: Some(config_a),
            },
            registry.clone(),
            generations,
        )
        .await;
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Disconnected { config, last_error: None })
                if config.disabled && mobile_mcp_config_unchanged(config, &disabled)
        ));
        assert_eq!(transport.connect_calls.load(Ordering::SeqCst), 0);
        assert!(generation_disabled > generation_a);
    });
}

#[test]
fn mobile_mcp_reload_revert_invalidates_a_pending_change() {
    let registry = Arc::new(McpRegistry::new(Arc::new(
        crate::mobile::mcp_transport::MobileMcpTransport::new(Arc::new(
            platform_common::RemoteMcpTransport::new(),
        )),
    )));
    let config_a = mobile_reload_test_config("http://127.0.0.1:1/a");
    let config_b = mobile_reload_test_config("http://127.0.0.1:1/b");
    registry.connections.blocking_write().insert(
        "remote".into(),
        mcp::connection::McpConnectionState::Disconnected {
            config: config_a.clone(),
            last_error: None,
        },
    );
    let generations = Arc::new(StdMutex::new(HashMap::new()));
    let (generation_a, _) =
        mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
    let (generation_b, changed) =
        mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_b));
    assert!(changed);
    assert!(generation_b > generation_a);

    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let connections = registry.connections.write().await;
        let job = tokio::spawn(mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Connect {
                name: "remote".into(),
                generation: generation_b,
                desired: config_b,
                previous: Some(config_a.clone()),
            },
            registry.clone(),
            generations.clone(),
        ));
        // The job has acquired the lifecycle lock and is waiting on the
        // state writer. Reverting disk intent must invalidate it before
        // the writer is released, even though visible state is still A.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(!job.is_finished());
        let (generation_reverted, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        assert!(changed);
        assert!(generation_reverted > generation_b);
        drop(connections);
        job.await.expect("stale change job");
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Disconnected { config, .. })
                if mobile_mcp_config_unchanged(config, &config_a)
        ));
    });
}

#[test]
fn mobile_mcp_reload_restore_invalidates_a_pending_delete() {
    let registry = Arc::new(McpRegistry::new(Arc::new(
        crate::mobile::mcp_transport::MobileMcpTransport::new(Arc::new(
            platform_common::RemoteMcpTransport::new(),
        )),
    )));
    let config_a = mobile_reload_test_config("http://127.0.0.1:1/a");
    registry.connections.blocking_write().insert(
        "remote".into(),
        mcp::connection::McpConnectionState::Disconnected {
            config: config_a.clone(),
            last_error: None,
        },
    );
    let generations = Arc::new(StdMutex::new(HashMap::new()));
    let (generation_a, _) =
        mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
    let (generation_deleted, changed) =
        mobile_mcp_record_reload_intent(&generations, "remote", None);
    assert!(changed);
    assert!(generation_deleted > generation_a);

    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.block_on(async {
        let connections = registry.connections.write().await;
        let job = tokio::spawn(mobile_mcp_run_reload_job(
            MobileMcpReloadJob::Remove {
                name: "remote".into(),
                generation: generation_deleted,
                expected: config_a.clone(),
            },
            registry.clone(),
            generations.clone(),
        ));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(!job.is_finished());
        let (generation_restored, changed) =
            mobile_mcp_record_reload_intent(&generations, "remote", Some(&config_a));
        assert!(changed);
        assert!(generation_restored > generation_deleted);
        drop(connections);
        job.await.expect("stale delete job");
        assert!(matches!(
            registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Disconnected { config, .. })
                if mobile_mcp_config_unchanged(config, &config_a)
        ));
    });
}

#[test]
fn mobile_mcp_reload_pending_generations_are_nonblocking_and_cas_guarded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(tmp.path());
    let settings_path = cfg.lingxi_home.join("mcp-config.json");
    std::fs::create_dir_all(&cfg.lingxi_home).expect("create mobile home");
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());
    let old_config = McpServerConfig {
        name: "remote".into(),
        spec: lingxi_core::host::McpTransportSpec::Http {
            url: "http://127.0.0.1:1/old".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: None,
        },
        scope: McpConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        discovery_cache: None,
        config_error: None,
        metadata: Default::default(),
    };
    let write_oauth_config = |url: &str| {
        std::fs::write(
                &settings_path,
                format!(
                    r#"{{"mcpServers":{{"remote":{{"type":"http","url":"{url}","oauth":{{"clientId":"mobile-test"}}}}}}}}"#
                ),
            )
            .expect("write OAuth MCP settings");
    };
    let seed_pending = |handle: &MobileEngineHandle| {
        handle.runtime().block_on(async {
            handle.inner.mcp_registry.connections.write().await.insert(
                "remote".into(),
                mcp::connection::McpConnectionState::AwaitingOAuth {
                    config: old_config.clone(),
                    callback_port: 43123,
                },
            );
        });
    };
    // A fake OAuth completion has the same ownership obligation as a real
    // connection task: compare and update the old pending state under one
    // writer. An unconditional insert can overwrite a completed reload after
    // its job has exited, leaving no task to restore the latest configuration.
    async fn complete_old_oauth(registry: &McpRegistry, old_config: &McpServerConfig) -> bool {
        let mut connections = registry.connections.write().await;
        let Some(state) = connections.get_mut("remote") else {
            return false;
        };
        if !matches!(
            state,
            mcp::connection::McpConnectionState::AwaitingOAuth { config, callback_port }
                if *callback_port == 43123 && mobile_mcp_config_unchanged(config, old_config)
        ) {
            return false;
        }
        *state = mcp::connection::McpConnectionState::Disconnected {
            config: old_config.clone(),
            last_error: None,
        };
        true
    }
    seed_pending(&handle);
    // Exercise completion while the fake still owns the slot, independently
    // of how Tokio schedules the reload jobs below.
    handle.runtime().block_on(async {
        assert!(complete_old_oauth(&handle.inner.mcp_registry, &old_config).await);
        assert!(matches!(
            handle.inner.mcp_registry.connections.read().await.get("remote"),
            Some(mcp::connection::McpConnectionState::Disconnected { config, last_error: None })
                if mobile_mcp_config_unchanged(config, &old_config)
        ));
    });
    seed_pending(&handle);
    let (completion_started, completion_ready) = tokio::sync::oneshot::channel();
    let (completion_release, completion_wait) = tokio::sync::oneshot::channel();
    let completion = handle.runtime().spawn({
        let registry = handle.inner.mcp_registry.clone();
        let old_config = old_config.clone();
        async move {
            completion_started.send(()).expect("report mock task ready");
            completion_wait.await.expect("release old OAuth completion");
            complete_old_oauth(&registry, &old_config).await
        }
    });
    handle.runtime().block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(2), completion_ready)
            .await
            .expect("old OAuth mock task starts")
            .expect("old OAuth mock task reports ready");
    });
    assert!(!completion.is_finished());

    write_oauth_config("http://127.0.0.1:1/first");
    let first_started = std::time::Instant::now();
    handle.runtime().block_on(handle.reload_configured_mcp());
    assert!(
        first_started.elapsed() < std::time::Duration::from_secs(1),
        "reload must not await a pending OAuth connection"
    );

    let first_generation = handle
        .inner
        .mcp_reload_generations
        .lock()
        .expect("reload generations")
        .get("remote")
        .expect("first reload intent recorded")
        .generation;

    write_oauth_config("http://127.0.0.1:1/second");
    let second_desired = mobile_mcp_preflight(
        mcp::load_mcp_servers(
            &handle.firer_cfg.cwd.join(".mcp.json"),
            &settings_path,
            &handle.firer_cfg.cwd,
        ),
        handle.inner.oauth_supported,
    )
    .into_iter()
    .find(|config| config.name == "remote")
    .expect("second OAuth MCP config");
    handle.runtime().block_on(handle.reload_configured_mcp());

    let second_generation = {
        let generations = handle
            .inner
            .mcp_reload_generations
            .lock()
            .expect("reload generations");
        let intent = generations
            .get("remote")
            .expect("second reload intent recorded");
        assert!(mobile_mcp_config_unchanged(
            intent.desired.as_ref().expect("second desired config"),
            &second_desired
        ));
        assert!(intent.generation > first_generation);
        intent.generation
    };
    assert!(!super::mobile_mcp_reload_generation_is_current(
        &handle.inner.mcp_reload_generations,
        "remote",
        first_generation
    ));
    assert!(super::mobile_mcp_reload_generation_is_current(
        &handle.inner.mcp_reload_generations,
        "remote",
        second_generation
    ));

    // Both reload intents are recorded while the old mock task is held at its
    // release barrier. Only the latest generation may settle the second config.
    handle.runtime().block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let state = handle
                    .inner
                    .mcp_registry
                    .connections
                    .read()
                    .await
                    .get("remote")
                    .cloned();
                if state.as_ref().is_some_and(|state| {
                    mobile_mcp_config_unchanged(state.config(), &second_desired)
                        && !mobile_mcp_state_is_transitional(state)
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("latest reload generation settles");
        let state = handle
            .inner
            .mcp_registry
            .connections
            .read()
            .await
            .get("remote")
            .cloned()
            .expect("latest config remains visible");
        assert!(mobile_mcp_config_unchanged(state.config(), &second_desired));

        // Force the formerly flaky ordering: the latest config is already
        // settled before the old OAuth task reports completion. The stale
        // fake must leave the entire installed state untouched.
        let settled = format!("{state:?}");
        assert!(!completion.is_finished());
        completion_release
            .send(())
            .expect("release late completion");
        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(2), completion)
                .await
                .expect("old OAuth mock completion settles")
                .expect("old OAuth mock task joins")
        );
        let after_completion = handle.inner.mcp_registry.connections.read().await;
        assert_eq!(
            format!(
                "{:?}",
                after_completion
                    .get("remote")
                    .expect("latest state retained")
            ),
            settled,
            "late fake OAuth completion must not overwrite the latest reload"
        );
    });
}

#[test]
fn mobile_mcp_reload_deleted_pending_generation_is_nonblocking() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(tmp.path());
    std::fs::create_dir_all(&cfg.lingxi_home).expect("create mobile home");
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());
    let old_config = McpServerConfig {
        name: "remote".into(),
        spec: lingxi_core::host::McpTransportSpec::Http {
            url: "http://127.0.0.1:1/pending".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: None,
        },
        scope: McpConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        discovery_cache: None,
        config_error: None,
        metadata: Default::default(),
    };
    handle.runtime().block_on(async {
        handle.inner.mcp_registry.connections.write().await.insert(
            "remote".into(),
            mcp::connection::McpConnectionState::AwaitingOAuth {
                config: old_config.clone(),
                callback_port: 43124,
            },
        );
    });

    let started = std::time::Instant::now();
    handle.runtime().block_on(handle.reload_configured_mcp());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "deleted reload must not await pending OAuth"
    );
    handle.runtime().block_on(async {
        // A reload may already have removed the old entry. Completing the
        // mock OAuth state must not resurrect a server that was deleted.
        let mut connections = handle.inner.mcp_registry.connections.write().await;
        if let Some(state) = connections.get_mut("remote") {
            *state = mcp::connection::McpConnectionState::Disconnected {
                config: old_config,
                last_error: None,
            };
        }
        drop(connections);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !handle
                    .inner
                    .mcp_registry
                    .connections
                    .read()
                    .await
                    .contains_key("remote")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("deleted reload generation settles");
    });
}

#[test]
fn submit_attach_turn_preserves_terminal_state_when_replay_history_is_truncated() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        for (turn_id, state, reason) in [
            (79, TurnRecoveryStateDto::Completed, None),
            (80, TurnRecoveryStateDto::Failed, Some("provider_failed")),
            (81, TurnRecoveryStateDto::Cancelled, Some("explicit_cancel")),
        ] {
            handle
                .durable_turns
                .begin(
                    &session_id,
                    turn_id,
                    lingxi_core::types::utf16_json::Utf16JsonProjection::from("hello"),
                    None,
                    Vec::new(),
                )
                .expect("begin durable turn");
            handle
                .durable_turns
                .append_event(
                    &session_id,
                    turn_id,
                    r#"{"type":"text_delta","text":"one"}"#.to_string(),
                )
                .expect("first event");
            handle
                .durable_turns
                .append_event(
                    &session_id,
                    turn_id,
                    r#"{"type":"text_delta","text":"two"}"#.to_string(),
                )
                .expect("second event");
            handle
                .durable_turns
                .retain_events_from_for_test(&session_id, turn_id, 2)
                .expect("truncate replay prefix");
            handle
                .durable_turns
                .transition(
                    &session_id,
                    turn_id,
                    state.clone(),
                    false,
                    reason.map(str::to_string),
                )
                .expect("terminal transition");

            listener.received.lock().await.clear();
            handle
                .submit(ClientCommand::AttachTurn {
                    turn_id,
                    after_sequence: None,
                })
                .await
                .expect("attach terminal turn");

            let events = listener.received.lock().await.clone();
            assert!(matches!(
                events.as_slice(),
                [Ev::TurnRecoveryState { snapshot }]
                    if snapshot.session_id == session_id
                        && snapshot.turn_id == turn_id
                        && snapshot.state == state
                        && !snapshot.safe_to_resume
                        && snapshot.reason.as_deref() == Some("replay_history_truncated")
            ));
            assert_eq!(
                handle
                    .durable_turns
                    .load(&session_id, turn_id)
                    .expect("reload terminal turn")
                    .state,
                Some(state),
                "attach must not mutate a terminal checkpoint into waiting_for_user"
            );
        }
    });
}

#[test]
fn build_mobile_marks_builtin_workflow_guideline_default_when_unset() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> =
            handle.inner.orchestrator.clone();
        assert!(
            orch.dynamic_workflows_enabled().await,
            "mobile should default enableWorkflows to true when no tier sets it"
        );
        assert_eq!(
            orch.workflow_size_guideline().await,
            "medium",
            "mobile should retain the built-in workflowSizeGuideline default in session state"
        );
        assert!(
            orch.workflow_size_guideline_is_default().await,
            "an unset workflowSizeGuideline must remain marked as the built-in default"
        );
        assert!(
            !orch.workflow_size_guideline_managed().await,
            "mobile has no managed workflow-size tier"
        );
    });
}

#[test]
fn build_mobile_applies_explicit_workflow_settings_from_user_project_local() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let user_home = tmp.path().join("home").join(branding::DOT_DIR);
    let project_settings_dir = tmp.path().join(branding::DOT_DIR);
    std::fs::create_dir_all(&user_home).expect("create user settings dir");
    std::fs::create_dir_all(&project_settings_dir).expect("create project settings dir");
    std::fs::write(
        user_home.join("settings.json"),
        r#"{"workflowSizeGuideline":"small","enableWorkflows":true}"#,
    )
    .expect("write user settings");
    std::fs::write(
        project_settings_dir.join("settings.json"),
        r#"{"workflowSizeGuideline":"large","enableWorkflows":true}"#,
    )
    .expect("write project settings");
    std::fs::write(
        project_settings_dir.join("settings.local.json"),
        r#"{"workflowSizeGuideline":"medium","enableWorkflows":false}"#,
    )
    .expect("write local settings");

    let mut cfg = test_config(tmp.path());
    cfg.lingxi_home = user_home;
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());

    handle.runtime().block_on(async {
        let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> =
            handle.inner.orchestrator.clone();
        assert!(
            !orch.dynamic_workflows_enabled().await,
            "the local enableWorkflows=false override must disable workflows for the session"
        );
        assert_eq!(
            orch.workflow_size_guideline().await,
            "medium",
            "the last workflowSizeGuideline tier should win"
        );
        assert!(
            !orch.workflow_size_guideline_is_default().await,
            "an explicit medium setting must not be mistaken for the built-in default"
        );
        assert!(
            !orch.workflow_size_guideline_managed().await,
            "mobile should publish workflow size as unmanaged"
        );
    });
}

#[test]
fn submit_lists_and_loads_nested_workflow_agents() {
    use futures_util::StreamExt as _;
    use lingxi_core::host::subagent_spawn::SubagentSpawner as _;

    struct TranscriptModel;
    #[async_trait::async_trait]
    impl agent::SubagentApiClient for TranscriptModel {
        async fn stream(
            &self,
            _request: agent::api::SubagentApiRequest,
        ) -> Result<
            futures_util::stream::BoxStream<
                'static,
                Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
            >,
            llm_runtime::LlmError,
        > {
            let response = llm_runtime::HistoryResponse {
                id: "nested-workflow-child".into(),
                model: "claude-sonnet-5".into(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "nested workflow child".into(),
                    cache_control: None,
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                stop_details: None,
                usage: llm_runtime::ExecutionUsage::default(),
                cost: None,
                provider_metadata: lingxi_llm_client::protocol::JsonPayload::default(),
                resumable: None,
            };
            let events = llm_runtime::stream_accumulator::response_to_stream_events(response);
            Ok(futures_util::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    struct NoToolUse;
    #[async_trait::async_trait]
    impl lingxi_core::host::ToolInvoker for NoToolUse {
        async fn invoke(
            &self,
            _: &str,
            _: Utf16JsonProjection,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<Utf16JsonProjection, lingxi_core::host::tool_invoker::ToolInvokerError>
        {
            unreachable!("the scripted child response does not call tools")
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::BudgetEnforcerHandle for NoToolUse {
        async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::BudgetError> {
            Ok(())
        }

        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.inner.orchestrator.current_session_id().await;
        let subagents_dir = orchestrator::transcript_paths::subagents_dir(
            &handle.lingxi_home,
            &handle.session_cwd(),
            &session_id.as_uuid().to_string(),
        );
        let dir = subagents_dir.join("workflows").join("wf_nested");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create nested workflow dir");

        let route_context: Arc<dyn agent::model_resolution::ModelResolutionContextProvider> =
            Arc::new(|model: &str, profile: Option<&str>| {
                Ok(agent::model_resolution::ModelResolutionContext {
                    route: agent::model_resolution::ModelRouteFacts {
                        model: model.to_string(),
                        profile: profile.map(str::to_owned),
                        provider: Some(agent::model_resolution::ModelProviderKind::FirstParty),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            });
        let transcript_fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::PosixFileSystem::new(tmp.path().to_path_buf()),
        );
        let spawner = agent::PoolSubagentSpawner::new(Arc::new(agent::StateMachinePool::new(
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
            2,
        )))
        .with_api_client(Arc::new(TranscriptModel))
        .with_default_model("claude-sonnet-5")
        .with_model_resolution_context_provider(route_context)
        .with_hook_context(session_id, tmp.path().to_path_buf(), Some(subagents_dir))
        .with_transcript_fs(transcript_fs)
        .with_session_interactive(false);
        let result = agent::with_transcript_subdir_override(Some(dir.clone()), async {
            spawner
                .spawn(
                    lingxi_core::host::SubagentSpawnRequest {
                        parent_native_query_depth: None,
                        subagent_type: agent::builtins::WORKFLOW_SUBAGENT_TYPE.into(),
                        prompt: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                            r#""Nested workflow child""#,
                        )
                        .unwrap(),
                        name: Some("wf child".into()),
                        origin_session_id: Some(session_id),
                        ..Default::default()
                    },
                    lingxi_core::host::SubagentInheritance {
                        tool_invoker: Arc::new(NoToolUse),
                        budget: Arc::new(NoToolUse),
                    },
                )
                .await
        })
        .await
        .expect("run nested workflow child through the production Agent writer");
        let expected_agent_id = match result {
            lingxi_core::host::SubagentResult::Completed { agent_id, .. } => agent_id.to_string(),
            other => panic!("nested workflow child did not complete: {other:?}"),
        };
        let transcript_path = dir.join(format!("agent-{expected_agent_id}.jsonl"));
        let transcript = tokio::fs::read_to_string(&transcript_path)
            .await
            .expect("production Agent writer creates the nested transcript");
        let persisted_indexes = transcript
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|row| row.get("message_index").and_then(serde_json::Value::as_u64))
            .collect::<Vec<_>>();
        assert_eq!(persisted_indexes, vec![0, 1]);
        let metadata_path = std::path::PathBuf::from(format!("{}.meta", transcript_path.display()));
        let metadata: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(&metadata_path)
                .await
                .expect("read writer high-water sidecar"),
        )
        .expect("parse writer high-water sidecar");
        assert_eq!(metadata["next_message_index"], 2);

        handle
            .submit(ClientCommand::ListSessionAgents)
            .await
            .expect("list session agents");
        let listed = listener.received.lock().await.clone();
        assert!(listed.iter().any(|event| matches!(
            event,
            Ev::SessionAgentList { agents, .. }
                if agents.iter().any(|agent| agent.agent_id == expected_agent_id)
        )));

        handle
            .submit(ClientCommand::LoadSessionAgentTranscript {
                agent_id: expected_agent_id.clone(),
            })
            .await
            .expect("load nested session agent transcript");
        let loaded = listener.received.lock().await.clone();
        let (messages, next_message_index) = loaded
            .iter()
            .find_map(|event| match event {
                Ev::SessionAgentTranscript {
                    agent_id,
                    messages,
                    next_message_index,
                    ..
                } if agent_id == &expected_agent_id => Some((messages, next_message_index)),
                _ => None,
            })
            .expect("nested session-agent transcript event");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.message_index)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "loaded rows retain the production writer's stable stream indices"
        );
        assert_eq!(*next_message_index, 2);
        assert!(messages
            .iter()
            .any(|message| message.message.blocks.iter().any(|block| {
                matches!(
                    block,
                    client::protocol::message::MessageBlockDto::Text { text }
                        if text.display().contains("nested workflow child")
                )
            })));
    });
}

#[test]
fn submit_task_message_reaches_registry_after_workspace_trust() {
    for trusted in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path());
        cfg.workspace_trusted = trusted;
        let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
        handle.runtime().block_on(async {
            let error = handle
                .submit(ClientCommand::TaskMessage {
                    task_id: "missing-task".into(),
                    message: "continue".into(),
                })
                .await
                .unwrap_err();
            if trusted {
                assert!(
                    error.to_string().contains("task message failed:"),
                    "trusted request must reach the task registry: {error}"
                );
            } else {
                assert!(error.to_string().contains("Trust this workspace"));
            }
        });
    }
}

#[test]
fn submit_task_message_enters_the_real_human_inbox_without_using_model_send() {
    struct ModelInbox;
    #[async_trait::async_trait]
    impl lingxi_core::host::task_registry::TaskMessageReceiver for ModelInbox {
        async fn send(
            &self,
            _: lingxi_core::types::utf16_json::Utf16JsonProjection,
        ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
            panic!("human command must use the dedicated human inbox");
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path());
    cfg.workspace_trusted = true;
    let (handle, listener) = build_submit_handle_with_config(cfg, tmp.path());
    handle.runtime().block_on(async {
            use lingxi_core::host::task_registry::{TaskCreateInput, TaskRegistryHandle};
            let registry = handle.inner.task_registry.as_ref();
            let task = TaskRegistryHandle::create(registry, TaskCreateInput { task_type: "local_agent".into(), description: "active agent fixture".into() }).await.unwrap();
            let agent_id = lingxi_core::types::AgentId::new();
            registry.bind_agent_id(&task.task_id, agent_id).await.unwrap();
            registry.bind_agent_message_receiver(&task.task_id, Arc::new(ModelInbox)).await.unwrap();
            handle.submit(ClientCommand::TaskMessage { task_id: task.task_id.clone(), message: "  continue with care\nnext line".into() }).await.unwrap();
            let received = registry.take_human_task_messages_for(agent_id).await;
            assert_eq!(
                received
                    .iter()
                    .map(|message| message.string_units(""))
                    .collect::<Vec<_>>(),
                vec![Some("  continue with care\nnext line".encode_utf16().collect())]
            );
            assert!(listener.received.lock().await.iter().any(|event| matches!(event, Ev::SystemNotice { message, is_error: false } if client_text_contains(message, &task.task_id))));
        });
}

#[test]
fn mobile_human_message_reconstructs_stopped_agent_with_original_identity_and_history() {
    use std::io::{Read, Write};
    struct NoWork;
    #[async_trait::async_trait]
    impl lingxi_core::host::ToolInvoker for NoWork {
        async fn invoke(
            &self,
            _: &str,
            _: Utf16JsonProjection,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<Utf16JsonProjection, lingxi_core::host::tool_invoker::ToolInvokerError>
        {
            unreachable!("fixture model never calls tools")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::BudgetEnforcerHandle for NoWork {
        async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let base = format!("http://{}", server.local_addr().unwrap());
    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    let server_thread = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut socket = loop {
            match server.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(error) => panic!("local provider was never called: {error}"),
            }
        };
        // The listener above is non-blocking, and on macOS/BSD an accepted
        // socket INHERITS O_NONBLOCK. `set_read_timeout` is meaningless on a
        // non-blocking socket — `read` returns `WouldBlock` the instant no
        // bytes are buffered rather than waiting — so the unwrap below
        // panicked with `Os { code: 35 }` whenever the request had not landed
        // yet. In isolation it always had; under full-suite load it had not.
        // Put the socket back into blocking mode so the 5s timeout is the
        // thing that actually bounds the read.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let n = socket.read(&mut chunk).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse().unwrap())
            })
            .expect("JSON request content length");
        while bytes.len() < header_end + length {
            let mut chunk = [0; 4096];
            let n = socket.read(&mut chunk).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&chunk[..n]);
        }
        let body: serde_json::Value =
            serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        let _ = captured_tx.send(body);
        let events = [
            serde_json::json!({"type":"message_start","message":{"id":"fixture-response","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"continued"}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}),
            serde_json::json!({"type":"message_stop"}),
        ];
        let body = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = test_config(tmp.path());
    cfg.workspace_trusted = true;
    cfg.api_base = base;
    cfg.api_key = "test-fixture-key".into();
    cfg.default_model = "claude-sonnet-4-5".into();
    let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
    // Observe the resumed child itself; no unrelated main-loop wake request
    // is needed to prove this targeted producer reaches a real model turn.
    handle.task_notification_watcher.abort();
    handle.runtime().block_on(async {
        use lingxi_core::host::task_registry::{TaskCreateInput, TaskRegistryHandle};
        let registry = handle.inner.task_registry.as_ref();
        let task = TaskRegistryHandle::create(
            registry,
            TaskCreateInput {
                task_type: "local_agent".into(),
                description: "stopped original agent".into(),
            },
        )
        .await
        .unwrap();
        let id = lingxi_core::types::AgentId::new();
        registry.bind_agent_id(&task.task_id, id).await.unwrap();
        let session_id = handle.inner.orchestrator.current_session_id().await;
        let dir = orchestrator::transcript_paths::subagents_dir(
            &handle.lingxi_home,
            &handle.session_cwd(),
            &session_id.as_uuid().to_string(),
        );
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let transcript = session::forked_skill::agent_transcript_path(&dir, &id.to_string());
        let previous = lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "original review context".into(),
        );
        tokio::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"model":"claude-sonnet-4-5","message":previous})
            ),
        )
        .await
        .unwrap();
        TaskRegistryHandle::register_agent_resume_recipe(
            registry,
            &task.task_id,
            lingxi_core::host::SubagentSpawnRequest {
                parent_native_query_depth: None,
                subagent_type: "general-purpose".into(),
                prompt: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    r#""do not replay this initial prompt""#,
                )
                .unwrap(),
                cwd: Some(tmp.path().display().to_string()),
                model: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            lingxi_core::host::SubagentInheritance {
                tool_invoker: Arc::new(NoWork),
                budget: Arc::new(NoWork),
            },
        )
        .await
        .unwrap();
        handle
            .submit(ClientCommand::TaskStop {
                task_id: task.task_id.clone(),
            })
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.submit(ClientCommand::TaskMessage {
                task_id: task.task_id.clone(),
                message: "  finish the review".into(),
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let body = match tokio::time::timeout(std::time::Duration::from_secs(5), captured_rx).await
        {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => panic!("provider request capture channel closed: {error}"),
            Err(error) => {
                let state = registry.get(&task.task_id).await;
                let output =
                    lingxi_core::host::task_registry::TaskRegistryHandle::output(
                        registry,
                        &task.task_id,
                        Some(0),
                    )
                    .await;
                panic!(
                    "no provider request arrived within 5s after human-resume startup: {error}; task state: {state:?}; task output: {output:?}"
                );
            }
        };
        assert_eq!(
            body["stream"], true,
            "restoration must reach an actual streaming model request"
        );
        let messages = body["messages"].to_string();
        assert!(
            messages.contains("original review context"),
            "restored turn lost prior transcript: {messages}"
        );
        assert!(
            messages.contains("finish the review"),
            "human follow-up did not reach provider: {messages}"
        );
        assert!(!messages.contains("do not replay this initial prompt"));
        let tasks::TaskState::LocalAgent(restored) = registry.get(&task.task_id).await.unwrap()
        else {
            panic!("restored task changed type")
        };
        assert_eq!(restored.agent_id, id);
    });
    server_thread.join().unwrap();
}

#[test]
fn submit_task_stop_skips_second_workflow_status_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let task_id = handle
            .inner
            .task_registry
            .create(
                tasks::TaskType::LocalWorkflow,
                tasks::TaskSpawnInput::LocalWorkflow {
                    session_uuid: None,
                    workflow_id: "workflow".to_string(),
                    script: "return null;".to_string(),
                    resume_from_run_id: None,
                    args: None,
                    run_id: None,
                    invocation_mode: Some("inline".to_string()),
                    workflow_source: Some("inline".to_string()),
                    script_is_verbatim_builtin: Some(false),
                    transcript_subdir: None,
                    launched_from_subagent: false,
                    tool_use_id: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                },
                Utf16JsonProjection::from("workflow"),
            )
            .await
            .expect("create workflow placeholder");
        listener
            .on_event(Ev::TaskStatusChanged {
                task_id: task_id.clone(),
                status: client::protocol::listings::TaskStatusDto::Cancelled,
                origin_session_id: None,
                error: None,
            })
            .await;

        handle
            .submit(ClientCommand::TaskStop {
                task_id: task_id.clone(),
            })
            .await
            .expect("task stop succeeds");

        let events = listener.received.lock().await.clone();
        let stop_events = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Ev::TaskStatusChanged { task_id: event_task_id, .. }
                        if event_task_id == &task_id
                )
            })
            .count();
        assert_eq!(
            stop_events, 1,
            "workflow TaskStop should rely on the sink emission, not emit a second status event"
        );
    });
}

#[test]
fn provider_credentials_round_trip_through_mobile_submit() {
    use client::protocol::commands::ProviderCredentialSecretDto;

    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle_with_secure_store(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SetProviderCredential {
                operation_id: 11,
                provider_id: "openai".into(),
                credential: ProviderCredentialSecretDto::new("sk-test-secret".into()),
            })
            .await
            .expect("set provider credential");
        handle
            .submit(ClientCommand::ListProviderCredentials {
                operation_id: 12,
                provider_ids: vec!["openai".into()],
                preview_provider_ids: vec!["openai".into()],
            })
            .await
            .expect("list provider credentials");
        handle
            .submit(ClientCommand::DeleteProviderCredential {
                operation_id: 13,
                provider_id: "openai".into(),
            })
            .await
            .expect("delete provider credential");

        let events = listener.received.lock().await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::ProviderCredentialStatus {
                operation_id: 11,
                configured_provider_ids,
                storage_encrypted: true,
                error: None,
                ..
            } if configured_provider_ids == &vec!["openai".to_string()]
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::ProviderCredentialStatus {
                operation_id: 12,
                configured_provider_ids,
                storage_encrypted: true,
                error: None,
                ..
            } if configured_provider_ids == &vec!["openai".to_string()]
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::ProviderCredentialStatus {
                operation_id: 13,
                configured_provider_ids,
                storage_encrypted: true,
                error: None,
                ..
            } if configured_provider_ids.is_empty()
        )));
    });
}

#[tokio::test]
async fn retained_mobile_engines_observe_other_engine_credential_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(FakeEncryptedStore::default());
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()).with_secure_storage(storage));
    let build = || {
        build_mobile(
            test_config(tmp.path()),
            platform.clone(),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
        )
    };
    let first = build().await.expect("first retained engine");
    first
        .credentials
        .set_provider_key("anthropic", "old-key")
        .await
        .unwrap();
    let second = build().await.expect("second retained engine");
    let captured = second
        .credentials
        .get_anthropic_api_key()
        .await
        .unwrap()
        .unwrap();

    first
        .credentials
        .set_provider_key("anthropic", "new-key")
        .await
        .unwrap();
    assert_eq!(
        second
            .credentials
            .get_anthropic_api_key()
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "new-key"
    );
    first
        .credentials
        .delete_provider_key("anthropic")
        .await
        .unwrap();
    assert!(second
        .credentials
        .get_anthropic_api_key()
        .await
        .unwrap()
        .is_none());
    assert_eq!(captured.expose_secret(), "old-key");
}

/// The picker must offer ONLY providers the routing allowlist kept.
///
/// `emit_listing` sourced its rows from `list_model_listings()` — the STATIC
/// llm-runtime catalog — while `apply_mobile_profile_allowlist` had already
/// stripped the un-listed profiles out of the live client config. A user who
/// had configured only DeepSeek was still shown every Anthropic/OpenAI/Kimi
/// row, and picking one set a profile the config no longer contained, so the
/// turn failed against a provider that was never connected.
#[test]
fn model_listing_offers_only_allowlisted_providers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = MobileConfig {
        idle_compaction_enabled: false,
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(branding::DOT_DIR),
        routing: Some(serde_json::json!({ "mobileEnabledProfiles": ["deepseek"] })),
        default_model: "deepseek/deepseek-flash".to_string(),
        ..MobileConfig::default()
    };
    let (handle, listener) = build_submit_handle_with_config(cfg, tmp.path());

    handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::ListModels)
                .await
                .expect("submit(ListModels) ok");
            let events = listener.received.lock().await.clone();
            let models = events
                .iter()
                .find_map(|event| match event {
                    Ev::ModelList { models, .. } => Some(models.clone()),
                    _ => None,
                })
                .expect("ModelList must be emitted");
            let providers = events
                .iter()
                .find_map(|event| match event {
                    Ev::ProviderModelCatalog { providers } => Some(providers.clone()),
                    _ => None,
                })
                .expect("ProviderModelCatalog must be emitted");

            assert!(
                models.iter().all(|m| m.starts_with("deepseek/")),
                "allowlisted-out providers leaked into the picker: {models:?}"
            );
            assert!(
                models.iter().any(|m| m == "deepseek/deepseek-flash"),
                "the allowlisted provider's curated models must still be offered: {models:?}"
            );
            assert!(
                providers.iter().any(|provider| provider.provider_id == "anthropic"),
                "settings catalog must retain built-in providers before the mobile allowlist: {providers:?}"
            );
            assert!(
                providers.iter().any(|provider| provider.provider_id == "deepseek"),
                "settings catalog must retain the configured provider: {providers:?}"
            );
        });
}

/// …and must REFUSE to switch to one that was allowlisted out, instead of
/// parsing it as a bare id and poisoning `session.model` with a reference no
/// provider serves (which the transcript then persists).
#[test]
fn set_model_rejects_a_provider_the_allowlist_removed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = MobileConfig {
        idle_compaction_enabled: false,
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(branding::DOT_DIR),
        routing: Some(serde_json::json!({ "mobileEnabledProfiles": ["anthropic"] })),
        ..MobileConfig::default()
    };
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());

    handle.runtime().block_on(async {
        let before: Arc<dyn lingxi_core::host::OrchestratorHandle> =
            handle.inner.orchestrator.clone();
        let before = before.get_status_snapshot().await;

        let result = handle
            .submit(ClientCommand::SetModel {
                model: "deepseek/deepseek-flash".into(),
            })
            .await;
        assert!(
            result.is_err(),
            "switching to a non-configured provider must be rejected, got {result:?}"
        );

        let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> =
            handle.inner.orchestrator.clone();
        let after = orch.get_status_snapshot().await;
        assert_eq!(
            (after.model, after.model_profile),
            (before.model, before.model_profile),
            "a rejected switch must leave the session model untouched"
        );
    });
}

/// `NewSession { model }` validates the model BEFORE `clear_session`, so a
/// model no configured provider serves is refused with the OLD session still
/// intact — rather than destroying it and then failing.
#[test]
fn new_session_with_an_unroutable_model_is_refused_without_clearing_the_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = MobileConfig {
        idle_compaction_enabled: false,
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(branding::DOT_DIR),
        routing: Some(serde_json::json!({ "mobileEnabledProfiles": ["anthropic", "openai"] })),
        ..MobileConfig::default()
    };
    let (handle, _listener) = build_submit_handle_with_config(cfg, tmp.path());

    handle.runtime().block_on(async {
        let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> =
            handle.inner.orchestrator.clone();
        let before = orch.current_session_id().await;

        let result = handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: Some("deepseek/deepseek-flash".into()),
            })
            .await;
        assert!(result.is_err(), "expected rejection, got {result:?}");
        assert_eq!(
            orch.current_session_id().await,
            before,
            "the old session must survive a refused NewSession"
        );

        // A model the allowlist KEPT still starts a new session normally.
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: Some("openai/gpt-5.6-sol".into()),
            })
            .await
            .expect("a routable model must be accepted");
        assert_ne!(orch.current_session_id().await, before);
    });
}

/// A user-defined provider's models must be offered and selectable.
///
/// The picker read the STATIC llm-runtime catalog, which is assembled from
/// `builtin_presets()` and therefore contains no user provider at all — so a
/// proxy or self-hosted endpoint configured in settings appeared nowhere,
/// and its models could not be picked even though the router served them.
#[test]
fn a_user_defined_provider_is_offered_and_selectable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let providers = std::collections::BTreeMap::from([(
        "my-proxy".to_string(),
        serde_json::json!({
            "type": "openai",
            "baseUrl": "https://proxy.example/v1",
            "apiKeyEnv": "PROXY_API_KEY",
            "models": [{"id": "llama-3.3-70b"}, {"id": "internal-7b"}]
        }),
    )]);
    let cfg = MobileConfig {
        idle_compaction_enabled: false,
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(branding::DOT_DIR),
        provider_profiles: Some(providers),
        routing: Some(serde_json::json!({
            "mobileEnabledProfiles": ["my-proxy"]
        })),
        default_model: "my-proxy/llama-3.3-70b".to_string(),
        ..MobileConfig::default()
    };
    let (handle, listener) = build_submit_handle_with_config(cfg, tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ListModels)
            .await
            .expect("submit(ListModels) ok");
        let events = listener.received.lock().await.clone();
        let models = events
            .iter()
            .find_map(|event| match event {
                Ev::ModelList { models, .. } => Some(models.clone()),
                _ => None,
            })
            .expect("ModelList must be emitted");

        // A provider with no curated shortlist keeps its OWN catalog, so both
        // declared models are offered.
        assert!(
            models.iter().any(|m| m == "my-proxy/llama-3.3-70b"),
            "the custom provider's models must be offered: {models:?}"
        );
        assert!(
            models.iter().any(|m| m == "my-proxy/internal-7b"),
            "every model a non-curated provider declares is offered: {models:?}"
        );

        // …and picking one is accepted, with the profile preserved.
        handle
            .submit(ClientCommand::SetModel {
                model: "my-proxy/internal-7b".into(),
            })
            .await
            .expect("a custom provider's model must be selectable");
        let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> =
            handle.inner.orchestrator.clone();
        let snapshot = orch.get_status_snapshot().await;
        assert_eq!(snapshot.model, "internal-7b");
        assert_eq!(snapshot.model_profile.as_deref(), Some("my-proxy"));
    });
}

/// A fresh install: iOS ALWAYS emits `mobileEnabledProfiles`, and with no
/// provider configured that array is EMPTY — which
/// `apply_mobile_profile_allowlist` treats as fail-closed and strips every
/// profile. Nothing is routable in that state whatever we show, so the
/// picker keeps listing the catalog rather than rendering an empty sheet the
/// user cannot act on or explain. Pinned because it is a deliberate
/// exception to "only offer what is routable", not an oversight.
#[test]
fn empty_allowlist_still_lists_the_catalog_so_a_fresh_install_is_not_blank() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = MobileConfig {
        idle_compaction_enabled: false,
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(branding::DOT_DIR),
        routing: Some(serde_json::json!({ "mobileEnabledProfiles": [] })),
        ..MobileConfig::default()
    };
    let (handle, listener) = build_submit_handle_with_config(cfg, tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ListModels)
            .await
            .expect("submit(ListModels) ok");
        let events = listener.received.lock().await.clone();
        let models = events
            .iter()
            .find_map(|event| match event {
                Ev::ModelList { models, .. } => Some(models.clone()),
                _ => None,
            })
            .expect("ModelList must be emitted");
        assert!(
            models.len() > 1,
            "an unconfigured install must still see a catalog: {models:?}"
        );
    });
}

/// Mobile's flat model event must retain the provider profile in both the
/// catalog and the active selection so identical ids from different
/// providers remain independently selectable.
#[test]
fn submit_model_events_use_provider_qualified_ids() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ListModels)
            .await
            .expect("submit(ListModels) ok");

        let events = listener.received.lock().await.clone();
        let models = events
            .iter()
            .find_map(|event| match event {
                Ev::ModelList { models, .. } => Some(models),
                _ => None,
            })
            .expect("ModelList must be emitted");
        assert!(
            models.iter().any(|model| model == "openai/gpt-5.6-sol"),
            "OpenAI's shared model id must stay qualified: {models:?}"
        );
        assert!(
            models
                .iter()
                .any(|model| model == "github-copilot/gpt-5.6-sol"),
            "Copilot's shared model id must stay qualified: {models:?}"
        );

        handle
            .submit(ClientCommand::SetModel {
                model: "github-copilot/gpt-5.6-sol".into(),
            })
            .await
            .expect("submit(SetModel) ok");

        let events = listener.received.lock().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                Ev::ModelChanged { model, .. } if model == "github-copilot/gpt-5.6-sol"
            )),
            "ModelChanged must preserve the selected provider profile: {events:?}"
        );
    });
}

/// F3-05: `submit(SendPrompt)` MUST spawn the streaming turn on the
/// handle-owned runtime and RETURN PROMPTLY — it must not block for the
/// whole turn (results stream via the listener). We prove the call resolves
/// `Ok(())` without a `TurnEnded` having been delivered yet (the spawned turn
/// against the host fake `Platform` does not complete synchronously inside
/// the `submit` call).
#[test]
fn submit_send_prompt_returns_promptly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    let result = handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SendPrompt {
                text: "hello".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(7),
                visualization_context: None,
            })
            .await
    });
    assert!(
        result.is_ok(),
        "submit(SendPrompt) returned an error: {result:?}"
    );

    // The turn was SPAWNED, so `submit` returned before any `TurnEnded` was
    // delivered to the listener. (A `TurnStarted` may have been synthesized
    // synchronously, but the terminal `TurnEnded` must not have fired.)
    let saw_turn_ended = handle.runtime().block_on(async {
        listener
            .received
            .lock()
            .await
            .iter()
            .any(|e| matches!(e, Ev::TurnEnded { .. }))
    });
    assert!(
        !saw_turn_ended,
        "submit must return promptly — TurnEnded must not fire inside the call"
    );
}

struct PendingPromptStreamClient {
    captured: Arc<StdMutex<Vec<orchestrator::test_support_stream::CapturedStreamCall>>>,
    entered: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingPromptStreamClient {
    fn new(entered: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            captured: Arc::new(StdMutex::new(Vec::new())),
            entered: StdMutex::new(Some(entered)),
        }
    }

    fn captured_calls(&self) -> Vec<orchestrator::test_support_stream::CapturedStreamCall> {
        self.captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl orchestrator::StreamingApiClient for PendingPromptStreamClient {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<lingxi_core::types::ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        _request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures_util::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(orchestrator::test_support_stream::CapturedStreamCall {
                model: model.to_owned(),
                profile: profile.map(str::to_owned),
                effort_override: None,
                output_budget: None,
                native_anthropic_request_context: None,
                system: system.cloned(),
                messages,
                tools,
                query_source: query_source.to_owned(),
                skip_global_cache_for_system_prompt,
            });
        if let Some(entered) = self
            .entered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = entered.send(());
        }
        let stream: futures_util::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        > = Box::pin(futures_util::stream::pending());
        Ok(stream)
    }
}

#[derive(Default)]
struct MobileBashProcess {
    commands: StdMutex<Vec<String>>,
}

impl MobileBashProcess {
    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct InterruptibleMobileBashProcess {
    commands: StdMutex<Vec<String>>,
    started: Notify,
    release: Notify,
    joined: AtomicBool,
    killed_owner: StdMutex<Option<String>>,
}

impl InterruptibleMobileBashProcess {
    fn commands(&self) -> Vec<String> {
        self.commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl lingxi_core::host::ProcessRunner for InterruptibleMobileBashProcess {
    async fn run(
        &self,
        command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError> {
        let command = command.inner().args.last().cloned().unwrap_or_default();
        self.commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(command.clone());
        if command == "printf interrupted-mobile-bash" {
            self.started.notify_one();
            self.release.notified().await;
            self.joined.store(true, Ordering::Release);
        }
        Ok(mobile_linux_api::ProcessOutput {
            stdout: "partial stdout".into(),
            stderr: "partial stderr".into(),
            exit_code: 130,
            timed_out: false,
        })
    }

    async fn spawn_background(
        &self,
        _command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError> {
        Err(mobile_linux_api::ProcessError::Unsupported)
    }

    async fn kill(
        &self,
        _handle: &lingxi_core::host::ProcessHandle,
    ) -> Result<(), mobile_linux_api::ProcessError> {
        Ok(())
    }

    async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
        *self
            .killed_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(owner.to_string());
        self.release.notify_one();
        vec![42]
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn install_mobile_bash_probe_hooks(root: &std::path::Path, respond_to_bash: bool) {
    let settings_dir = root.join(".lingxi");
    std::fs::create_dir_all(&settings_dir).expect("create mobile Bash hook settings directory");
    let settings = serde_json::json!({
        "respondToBashCommands": respond_to_bash,
        "hooks": {
            "UserPromptSubmit": [{
                "hooks": [{ "type": "command", "command": "probe-user-prompt-submit" }]
            }],
            "PreToolUse": [{
                "hooks": [{ "type": "command", "command": "probe-pre-tool-use" }]
            }],
            "PostToolUse": [{
                "hooks": [{ "type": "command", "command": "probe-post-tool-use" }]
            }]
        }
    });
    std::fs::write(
        settings_dir.join("settings.json"),
        serde_json::to_vec(&settings).expect("serialize Bash hook settings"),
    )
    .expect("write mobile Bash hook settings");
}

#[async_trait]
impl lingxi_core::host::ProcessRunner for MobileBashProcess {
    async fn run(
        &self,
        command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError> {
        let inner = command.inner();
        self.commands
            .lock()
            .unwrap()
            .push(inner.args.last().cloned().unwrap_or_default());
        Ok(mobile_linux_api::ProcessOutput {
            stdout: "mobile stdout".into(),
            stderr: "mobile stderr".into(),
            exit_code: 7,
            timed_out: false,
        })
    }

    async fn spawn_background(
        &self,
        _command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError> {
        Err(mobile_linux_api::ProcessError::Unsupported)
    }

    async fn kill(
        &self,
        _handle: &lingxi_core::host::ProcessHandle,
    ) -> Result<(), mobile_linux_api::ProcessError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn exact_native_text_block(block: &lingxi_core::types::ContentBlock, expected: &str) -> bool {
    let expected_units = expected.encode_utf16().collect::<Vec<_>>();
    match block {
        lingxi_core::types::ContentBlock::Text { text, citations } => {
            text == expected
                && text.encode_utf16().eq(expected_units.iter().copied())
                && citations.is_none()
        }
        lingxi_core::types::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations,
        } => {
            utf16_code_units == &expected_units
                && text == expected
                && text.encode_utf16().eq(utf16_code_units.iter().copied())
                && citations.is_none()
        }
        _ => false,
    }
}

fn native_text_block_contains(block: &lingxi_core::types::ContentBlock, needle: &str) -> bool {
    match block {
        lingxi_core::types::ContentBlock::Text { text, .. } => text.contains(needle),
        lingxi_core::types::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            ..
        } => text.contains(needle) || String::from_utf16_lossy(utf16_code_units).contains(needle),
        _ => false,
    }
}

#[test]
fn mobile_bash_prompt_runs_shell_once_then_sends_native_rows_and_images_to_model() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, FakeProcessAdmittingSandbox,
    };
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(tmp.path());
    config.enable_mobile_linux_shell();
    install_mobile_bash_probe_hooks(tmp.path(), true);
    let process = Arc::new(MobileBashProcess::default());
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_process_runner(process.clone())
            .with_sandbox(Arc::new(FakeProcessAdmittingSandbox)),
    );
    let listener = Arc::new(FakeListener::default());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let handle = new_engine_with_streaming(
        config,
        platform,
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(pending.clone()),
    )
    .expect("build Mobile Shell host");

    handle.runtime().block_on(async {
        let prompt_images = vec![
            client::protocol::commands::ImageRefDto {
                media_type: "image/png".into(),
                base64: "cG5nLWZpcnN0".into(),
            },
            client::protocol::commands::ImageRefDto {
                media_type: "image/jpeg".into(),
                base64: "anBlZy1zZWNvbmQ=".into(),
            },
        ];
        handle
            .submit(ClientCommand::SendPrompt {
                text: "printf mobile-bash".into(),
                prompt_mode: Some(client::protocol::commands::PromptModeDto::Bash),
                images: prompt_images,
                turn_id: Some(806),
                visualization_context: None,
            })
            .await
            .expect("accept Bash prompt");

        for _ in 0..5_000 {
            if !process.commands().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(process.commands(), ["printf mobile-bash"]);
        assert_eq!(handle.permission_gate().pending_count().await, 0);
        let registered_hooks = handle.inner().orchestrator.list_hooks().await;
        assert!(registered_hooks.iter().any(|hook| hook.event == "UserPromptSubmit"));
        assert!(registered_hooks.iter().any(|hook| hook.event == "PreToolUse"));
        assert!(registered_hooks.iter().any(|hook| hook.event == "PostToolUse"));
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .expect("Bash result reaches the provider request")
            .expect("provider request signal");

        let calls = pending.captured_calls();
        assert_eq!(calls.len(), 1);
        let (bash_input_message_index, bash_content, is_meta, turn_companion) = calls[0]
            .messages
            .iter()
            .enumerate()
            .find_map(|(index, message)| match message {
                ConversationMessage::User {
                    content,
                    is_meta,
                    turn_companion,
                    ..
                } if content
                    .iter()
                    .any(|block| exact_native_text_block(
                        block,
                        "<bash-input>printf mobile-bash</bash-input>",
                    )) => Some((index, content, *is_meta, *turn_companion)),
                _ => None,
            })
            .expect("un-normalized Native Bash input reaches the streaming client");
        assert!(!is_meta, "the direct command is a user row");
        assert_eq!(turn_companion, None, "Bash input is not a skill companion");
        let input_block_index = bash_content
            .iter()
            .position(|block| {
                exact_native_text_block(block, "<bash-input>printf mobile-bash</bash-input>")
            })
            .expect("the host preserves the exact un-normalized Bash input block");
        assert_eq!(bash_content.len(), 3, "raw input is exactly two images then command");
        assert_eq!(input_block_index, 2);
        assert!(
            !bash_content.iter().any(|block| exact_native_text_block(
                block,
                "<bash-stdout>mobile stdout</bash-stdout><bash-stderr>mobile stderr</bash-stderr>",
            )),
            "the pending stream client sees the raw input row before API normalization"
        );
        let (bash_result_message_index, result_content, result_is_meta, result_turn_companion) =
            calls[0]
                .messages
                .iter()
                .enumerate()
                .find_map(|(index, message)| match message {
                    ConversationMessage::User {
                        content,
                        is_meta,
                        turn_companion,
                        ..
                    } if content.iter().any(|block| exact_native_text_block(
                        block,
                        "<bash-stdout>mobile stdout</bash-stdout><bash-stderr>mobile stderr</bash-stderr>",
                    )) => Some((index, content, *is_meta, *turn_companion)),
                    _ => None,
                })
                .expect("Bash output remains a separate queued user row before normalization");
        assert!(bash_input_message_index < bash_result_message_index);
        assert!(!result_is_meta, "the Bash result is a user row");
        assert_eq!(result_turn_companion, None);
        let result_block_index = result_content
            .iter()
            .position(|block| {
                exact_native_text_block(
                    block,
                    "<bash-stdout>mobile stdout</bash-stdout><bash-stderr>mobile stderr</bash-stderr>",
                )
            })
            .expect("the exact Bash output survives in its separate raw user row");
        assert_eq!(result_content.len(), 1);
        assert_eq!(result_block_index, 0);
        let images = bash_content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(images.len(), 2);
        let image_block_indices = bash_content
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                matches!(block, ContentBlock::Image { .. }).then_some(index)
            })
            .collect::<Vec<_>>();
        assert_eq!(image_block_indices.len(), images.len());
        assert!(
            image_block_indices
                .iter()
                .all(|index| *index < input_block_index),
            "Native carries pasted blocks before the Bash command"
        );
        assert!(matches!(
            images[0],
            lingxi_core::types::ImageSource::Base64 { media_type, data }
                if media_type == "image/png" && data == "cG5nLWZpcnN0"
        ));
        assert!(matches!(
            images[1],
            lingxi_core::types::ImageSource::Base64 { media_type, data }
                if media_type == "image/jpeg" && data == "anBlZy1zZWNvbmQ="
        ));

        handle
            .submit(ClientCommand::Cancel { turn_id: Some(806) })
            .await
            .expect("join the pending follow-up before leaving the test");
    });
}

#[test]
fn mobile_bash_interrupt_persists_native_rows_and_ends_once_without_query() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, FakeProcessAdmittingSandbox,
    };
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(tmp.path());
    config.enable_mobile_linux_shell();
    install_mobile_bash_probe_hooks(tmp.path(), true);
    let process = Arc::new(InterruptibleMobileBashProcess::default());
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_process_runner(process.clone())
            .with_sandbox(Arc::new(FakeProcessAdmittingSandbox)),
    );
    let listener = Arc::new(FakeListener::default());
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let handle = new_engine_with_streaming(
        config,
        platform,
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(pending.clone()),
    )
    .expect("build interruptible Mobile Shell host");

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SendPrompt {
                text: "printf interrupted-mobile-bash".into(),
                prompt_mode: Some(client::protocol::commands::PromptModeDto::Bash),
                images: Vec::new(),
                turn_id: Some(812),
                visualization_context: None,
            })
            .await
            .expect("accept Bash prompt");

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            process.started.notified(),
        )
        .await
        .expect("Shell process starts before cancellation");
        assert_eq!(process.commands(), ["printf interrupted-mobile-bash"]);

        handle
            .submit(ClientCommand::Cancel { turn_id: Some(812) })
            .await
            .expect("interrupt and join the owned Shell process");

        assert!(process.joined.load(Ordering::Acquire));
        assert!(process
            .killed_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
            .is_some_and(|owner| !owner.is_empty()));
        assert_eq!(process.commands(), ["printf interrupted-mobile-bash"]);
        assert!(pending.captured_calls().is_empty(), "interruption suppresses follow-up");

        let session_id = handle.active_session_id();
        let checkpoint = handle
            .durable_turns
            .load(&session_id, 812)
            .expect("cancelled Bash checkpoint");
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
        assert!(checkpoint.bash_result.as_ref().is_some_and(|result| result.interrupted));
        assert_eq!(checkpoint.bash_should_query, Some(false));

        let terminal_events = listener
            .received
            .lock()
            .await
            .iter()
            .filter(|event| matches!(event, ClientEvent::TurnEnded { .. }))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(terminal_events.len(), 1, "one terminal event: {terminal_events:?}");
        assert!(matches!(
            terminal_events.first(),
            Some(ClientEvent::TurnEnded {
                outcome: client::protocol::events::TurnOutcomeDto::Cancelled,
                ..
            })
        ));

        let history = handle.inner().orchestrator.snapshot_history().await;
        let rows = history
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::User {
                    content,
                    is_meta,
                    turn_companion,
                    ..
                } => Some((*is_meta, *turn_companion, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let row_contains = |content: &[ContentBlock], expected: &str| {
            content
                .iter()
                .any(|block| exact_native_text_block(block, expected))
        };
        let notice_index = rows
            .iter()
            .position(|(is_meta, _, content)| {
                *is_meta
                    && row_contains(
                        content,
                        "<system-reminder>The command below was run directly in Claude Code, not sent to you as a request, and its output goes straight to the user. It's recorded here as context for later messages.</system-reminder>",
                    )
            })
            .expect("Native direct-Bash notice survives interruption");
        let input_index = rows
            .iter()
            .position(|(_, _, content)| {
                row_contains(content, "<bash-input>printf interrupted-mobile-bash</bash-input>")
            })
            .expect("Native Bash input survives interruption");
        let interrupted_index = rows
            .iter()
            .position(|(_, _, content)| row_contains(content, "[Request interrupted by user]"))
            .expect("Native interruption marker is persisted");
        assert!(!rows[input_index].0, "the command stays a user row");
        assert!(!rows[interrupted_index].0, "the result stays a user row");
        assert_eq!(rows[notice_index].1, None);
        assert_eq!(rows[input_index].1, None);
        assert_eq!(rows[interrupted_index].1, None);
        assert!(notice_index < input_index && input_index < interrupted_index);
    });
}

#[test]
fn mobile_bash_with_responses_disabled_persists_native_rows_without_hooks_or_query() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, FakeProcessAdmittingSandbox,
    };
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(tmp.path());
    config.enable_mobile_linux_shell();
    install_mobile_bash_probe_hooks(tmp.path(), false);
    let process = Arc::new(MobileBashProcess::default());
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_process_runner(process.clone())
            .with_sandbox(Arc::new(FakeProcessAdmittingSandbox)),
    );
    let listener = Arc::new(FakeListener::default());
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let handle = new_engine_with_streaming(
        config,
        platform,
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(pending.clone()),
    )
    .expect("build Mobile Shell host");

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SendPrompt {
                text: "printf no-response".into(),
                prompt_mode: Some(client::protocol::commands::PromptModeDto::Bash),
                images: Vec::new(),
                turn_id: Some(807),
                visualization_context: None,
            })
            .await
            .expect("accept Bash prompt");

        let session_id = handle.active_session_id();
        for _ in 0..5_000 {
            let completed = handle
                .durable_turns
                .load(&session_id, 807)
                .ok()
                .is_some_and(|checkpoint| {
                    checkpoint.state == Some(TurnRecoveryStateDto::Completed)
                });
            if completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let checkpoint = handle
            .durable_turns
            .load(&session_id, 807)
            .expect("completed Bash checkpoint");
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Completed));
        assert_eq!(process.commands(), ["printf no-response"]);
        assert!(pending.captured_calls().is_empty(), "false suppresses model follow-up");
        let terminal_events = listener
            .received
            .lock()
            .await
            .iter()
            .filter(|event| matches!(event, ClientEvent::TurnEnded { .. }))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(terminal_events.len(), 1, "one terminal event: {terminal_events:?}");
        assert!(matches!(
            terminal_events.first(),
            Some(ClientEvent::TurnEnded {
                outcome: client::protocol::events::TurnOutcomeDto::EndTurn,
                ..
            })
        ));

        let history = handle.inner().orchestrator.snapshot_history().await;
        let rows = history
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::User {
                    content,
                    is_meta,
                    turn_companion,
                    ..
                } => Some((*is_meta, *turn_companion, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let row_contains = |content: &[ContentBlock], expected: &str| {
            content
                .iter()
                .any(|block| exact_native_text_block(block, expected))
        };
        let notice_index = rows
            .iter()
            .position(|(is_meta, _, content)| {
                *is_meta
                    && row_contains(
                        content,
                        "<system-reminder>The command below was run directly in Claude Code, not sent to you as a request, and its output goes straight to the user. It's recorded here as context for later messages.</system-reminder>",
                    )
            })
            .expect("Native no-response notice row");
        let input_index = rows
            .iter()
            .position(|(_, _, content)| row_contains(content, "<bash-input>printf no-response</bash-input>"))
            .expect("Native Bash input row");
        let result_index = rows
            .iter()
            .position(|(_, _, content)| {
                row_contains(
                    content,
                    "<bash-stdout>mobile stdout</bash-stdout><bash-stderr>mobile stderr</bash-stderr>",
                )
            })
            .expect("Native Bash output row");
        assert!(!rows[input_index].0, "the command stays a user row");
        assert!(!rows[result_index].0, "the no-response result stays a user row");
        assert_eq!(rows[notice_index].1, None);
        assert_eq!(rows[input_index].1, None);
        assert_eq!(rows[result_index].1, None);
        assert!(notice_index < input_index && input_index < result_index);
        assert!(rows.iter().all(|(_, _, content)| content.iter().all(|block| {
            !native_text_block_contains(block, "<bash-exit-code>")
        })));
    });
}

#[test]
fn mobile_bash_with_responses_disabled_cold_resume_never_executes_or_queries_again() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, FakeProcessAdmittingSandbox,
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(tmp.path());
    config.enable_mobile_linux_shell();
    install_mobile_bash_probe_hooks(tmp.path(), false);
    let process = Arc::new(MobileBashProcess::default());
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_process_runner(process.clone())
            .with_sandbox(Arc::new(FakeProcessAdmittingSandbox)),
    );
    let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();
    let first_pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let first = new_engine_with_streaming(
        config,
        platform,
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(first_pending.clone()),
    )
    .expect("build initial Mobile Shell host");

    let session_id = first.runtime().block_on(async {
        let session_id = first.active_session_id();
        first
            .inner()
            .session_writer
            .append_mobile_empty_session(&session_id, "cold Bash no-response", &first.session_cwd())
            .await
            .expect("anchor the session before restart");
        first
            .inner()
            .session_writer
            .append_session_mode(first.inner().session_mode.as_str())
            .await
            .expect("persist session mode");
        first
            .submit(ClientCommand::SendPrompt {
                text: "printf cold-no-response".into(),
                prompt_mode: Some(client::protocol::commands::PromptModeDto::Bash),
                images: Vec::new(),
                turn_id: Some(808),
                visualization_context: None,
            })
            .await
            .expect("accept no-response Bash prompt");

        for _ in 0..5_000 {
            let completed =
                first
                    .durable_turns
                    .load(&session_id, 808)
                    .ok()
                    .is_some_and(|checkpoint| {
                        checkpoint.state == Some(TurnRecoveryStateDto::Completed)
                    });
            if completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(
            first
                .durable_turns
                .load(&session_id, 808)
                .expect("completed first Bash turn")
                .state,
            Some(TurnRecoveryStateDto::Completed)
        );
        assert_eq!(process.commands(), ["printf cold-no-response"]);
        assert!(first_pending.captured_calls().is_empty());
        assert!(entered_rx.try_recv().is_err());
        session_id
    });
    drop(first);

    let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();
    let resumed_pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let resumed = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(
            HostFakePlatform::new(tmp.path().to_path_buf())
                .with_process_runner(process.clone())
                .with_sandbox(Arc::new(FakeProcessAdmittingSandbox)),
        ),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(resumed_pending.clone()),
    )
    .expect("rebuild Mobile Shell host after restart");
    resumed.runtime().block_on(async {
        resumed
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .expect("restore completed session");
        resumed
            .submit(ClientCommand::ResumeTurn { turn_id: 808 })
            .await
            .expect("completed no-response Bash stays terminal");
        assert!(resumed_pending.captured_calls().is_empty());
        assert!(entered_rx.try_recv().is_err());
        assert_eq!(process.commands(), ["printf cold-no-response"]);
        assert_eq!(
            resumed
                .durable_turns
                .load(&session_id, 808)
                .expect("restored Bash checkpoint")
                .state,
            Some(TurnRecoveryStateDto::Completed)
        );
    });
}

#[test]
fn mobile_send_prompt_cold_resume_replays_the_durable_prompt() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    use lingxi_core::types::{ContentBlock, ConversationMessage, ImageSource};
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };

    fn image_sources(messages: &[ConversationMessage]) -> Vec<ImageSource> {
        messages
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { source } => Some(source.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let text = "mobile cold-replay 🚀 prompt".to_owned();
    let mut prompt_units = text.encode_utf16().collect::<Vec<_>>();
    let isolated_surrogate_index = "mobile cold-replay 🚀".encode_utf16().count();
    prompt_units.insert(isolated_surrogate_index, 0xd800);
    let prompt_image_refs = vec![
        client::protocol::commands::ImageRefDto {
            media_type: "image/png".into(),
            base64: "aGVsbG8=".into(),
        },
        client::protocol::commands::ImageRefDto {
            media_type: "image/jpeg".into(),
            base64: "d29ybGQ=".into(),
        },
    ];
    let expected_image_refs = prompt_image_refs.clone();
    let expected_image_sources = vec![
        ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        },
        ImageSource::Base64 {
            media_type: "image/jpeg".into(),
            data: "d29ybGQ=".into(),
        },
    ];
    let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let first_listener = Arc::new(FakeListener::default());
    let first = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        first_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(pending.clone()),
    )
    .expect("build initial mobile engine");

    let first_runtime = first.runtime();
    let first_for_work = first.clone();
    let initial_prompt_units = prompt_units.clone();
    let first_image_refs = prompt_image_refs.clone();
    let expected_image_refs_for_first = expected_image_refs.clone();
    let expected_image_sources_for_first = expected_image_sources.clone();
    let session_id = first_runtime.block_on(async move {
        let first = first_for_work;
        let session_id = first
            .inner
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string();
        first
            .inner
            .session_writer
            .append_mobile_empty_session(&session_id, "cold prompt replay", &first.session_cwd())
            .await
            .expect("anchor the session before restart");
        first
            .inner
            .session_writer
            .append_session_mode(first.inner.session_mode.as_str())
            .await
            .expect("persist the session mode");

        first
            .submit(client::protocol::commands::ClientCommand::SendPrompt {
                text: ClientTextDto::from_code_units(initial_prompt_units.clone()),
                prompt_mode: None,
                images: first_image_refs.clone(),
                turn_id: Some(246),
                visualization_context: None,
            })
            .await
            .expect("submit user prompt");
        let wait_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let stream_entry_closed = match entered_rx.try_recv() {
                    Ok(()) => return Ok(()),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => true,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
                };
                if let Some(message) =
                    first_listener
                        .received
                        .lock()
                        .await
                        .iter()
                        .find_map(|event| match event {
                            ClientEvent::Error { message, .. } => Some(message.display()),
                            _ => None,
                        })
                {
                    return Err(message);
                }
                if stream_entry_closed {
                    return Err("stream entry signal closed".to_string());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        match wait_result {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                panic!("initial prompt failed before model call: {message}");
            }
            Err(_) => {
                let errors = first_listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .filter_map(|event| match event {
                        ClientEvent::Error { message, .. } => Some(message.display()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                panic!("model call opened before simulated restart; listener errors: {errors:?}");
            }
        }

        let first_calls = pending.captured_calls();
        assert_eq!(first_calls.len(), 1);
        assert!(
            first_calls[0].messages.iter().any(|message| matches!(
                message,
                ConversationMessage::User { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                            if utf16_code_units == &initial_prompt_units
                    ))
            )),
            "the first model request must preserve the exact prompt source units"
        );
        assert_eq!(
            image_sources(&first_calls[0].messages),
            expected_image_sources_for_first,
            "the initial model request preserves image bytes, MIME types, and source order"
        );

        let checkpoint = first
            .durable_turns
            .load(&session_id, 246)
            .expect("load the durable turn checkpoint");
        assert_eq!(
            checkpoint.prompt.string_units(""),
            Some(initial_prompt_units.clone())
        );
        assert_eq!(
            checkpoint.images, expected_image_refs_for_first,
            "the durable checkpoint preserves the original ordered image payload"
        );
        let metadata_path = first
            .lingxi_home
            .join("mobile-turns")
            .join(&session_id)
            .join("246.json");
        let metadata = std::fs::read_to_string(metadata_path).expect("read persisted prompt");
        let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&metadata)
            .expect("metadata remains valid raw JSON");
        assert_eq!(
            source.string_units("/prompt"),
            Some(initial_prompt_units.clone())
        );

        first
            .durable_turns
            .transition(
                &session_id,
                246,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("simulated_process_restart".into()),
            )
            .expect("mark the in-flight request recoverable before restart");
        session_id
    });
    drop(first);

    let resumed_stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("cold-prompt-replay", "test"),
        content_block_start_text(0),
        text_delta(0, "resumed"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let resumed_listener = Arc::new(FakeListener::default());
    let resumed = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        resumed_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(resumed_stream.clone()),
    )
    .expect("rebuild mobile engine after restart");

    resumed.runtime().block_on(async {
        resumed
            .submit(client::protocol::commands::ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .expect("restore the session containing the durable turn");
        resumed
            .submit(client::protocol::commands::ClientCommand::ResumeTurn { turn_id: 246 })
            .await
            .expect("resume from the exact stored prompt");
        let wait_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if !resumed_stream.captured_calls().await.is_empty() {
                        return None;
                    }
                    if let Some(message) = resumed_listener.received.lock().await.iter().find_map(
                        |event| match event {
                            ClientEvent::Error { message, .. } => Some(message.display()),
                            _ => None,
                        },
                    ) {
                        return Some(message);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
        match wait_result {
            Ok(Some(message)) => panic!("cold resume failed before model call: {message}"),
            Ok(None) => {}
            Err(_) => {
                let errors = resumed_listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .filter_map(|event| match event {
                        ClientEvent::Error { message, .. } => Some(message.display()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                panic!("cold resume reaches the model request; listener errors: {errors:?}");
            }
        }

        let calls = resumed_stream.captured_calls().await;
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].messages.iter().any(|message| matches!(
                message,
                ConversationMessage::User { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                            if utf16_code_units == &prompt_units
                    ))
            )),
            "the resumed model request must preserve the exact prompt source units"
        );
        assert_eq!(
            image_sources(&calls[0].messages),
            expected_image_sources,
            "cold resume preserves image bytes, MIME types, and source order"
        );
        assert_eq!(
            resumed
                .durable_turns
                .load(&session_id, 246)
                .expect("read resumed checkpoint")
                .prompt
                .string_units(""),
            Some(prompt_units.clone())
        );
        assert_eq!(
            resumed
                .durable_turns
                .load(&session_id, 246)
                .expect("read resumed checkpoint images")
                .images,
            expected_image_refs,
            "cold recovery retains the source image payload in checkpoint order"
        );
    });
}

#[test]
fn mobile_send_prompt_without_turn_id_is_durable_across_cold_resume() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    use lingxi_core::types::{ContentBlock, ConversationMessage, ImageSource};
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let prompt = "image prompt without durable turn id";
    let prompt_units = prompt.encode_utf16().collect::<Vec<_>>();
    let image_refs = vec![
        client::protocol::commands::ImageRefDto {
            media_type: "image/png".into(),
            base64: "aGVsbG8=".into(),
        },
        client::protocol::commands::ImageRefDto {
            media_type: "image/jpeg".into(),
            base64: "d29ybGQ=".into(),
        },
    ];
    let expected_images = vec![
        ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        },
        ImageSource::Base64 {
            media_type: "image/jpeg".into(),
            data: "d29ybGQ=".into(),
        },
    ];
    let initial_prompt_units = prompt_units.clone();
    let initial_expected_images = expected_images.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let first_listener = Arc::new(FakeListener::default());
    let engine = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        first_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(pending.clone()),
    )
    .expect("build mobile engine");

    let runtime = engine.runtime();
    let engine_for_work = engine.clone();
    let (session_id, turn_id) = runtime.block_on(async move {
        let session_id = engine_for_work
            .inner
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string();
        engine_for_work
            .inner
            .session_writer
            .append_mobile_empty_session(
                &session_id,
                "uncorrelated image prompt",
                &engine_for_work.session_cwd(),
            )
            .await
            .expect("anchor the session transcript");
        engine_for_work
            .inner
            .session_writer
            .append_session_mode(engine_for_work.inner.session_mode.as_str())
            .await
            .expect("persist the session mode");

        engine_for_work
            .submit(client::protocol::commands::ClientCommand::SendPrompt {
                text: ClientTextDto::from_code_units(initial_prompt_units.clone()),
                prompt_mode: None,
                images: image_refs.clone(),
                turn_id: None,
                visualization_context: None,
            })
            .await
            .expect("submit the prompt without a client turn id");
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .expect("the prompt should reach the provider")
            .expect("provider entry channel should remain open");

        let calls = pending.captured_calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].messages.iter().any(|message| matches!(
                message,
                ConversationMessage::User { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                            if utf16_code_units == &initial_prompt_units
                    ))
            )),
            "the direct provider request retains the original user text"
        );
        let actual_images = calls[0]
            .messages
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { source } => Some(source.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual_images, initial_expected_images,
            "a no-client-ID prompt preserves image bytes, MIME types, and order"
        );
        let turn_id = first_listener
            .received
            .lock()
            .await
            .iter()
            .find_map(|event| match event {
                ClientEvent::TurnStarted { turn_id } => *turn_id,
                _ => None,
            })
            .expect("the host reports its internally assigned durable turn ID");
        let checkpoint = engine_for_work
            .durable_turns
            .load(&session_id, turn_id)
            .expect("direct no-ID input has a durable checkpoint");
        assert_eq!(checkpoint.original_client_turn_id, None);
        assert_eq!(
            checkpoint.prompt.string_units(""),
            Some(initial_prompt_units.clone())
        );
        assert_eq!(checkpoint.images, image_refs);
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Running));
        assert_eq!(checkpoint.queue_delivery, QueueDeliveryState::Claimed);
        engine_for_work
            .durable_turns
            .transition(
                &session_id,
                turn_id,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("simulated_process_restart".into()),
            )
            .expect("mark the active request resumable before restart");
        (session_id, turn_id)
    });
    drop(engine);

    let resumed_stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("no-id-cold-replay", "test"),
        content_block_start_text(0),
        text_delta(0, "resumed"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let resumed_listener = Arc::new(FakeListener::default());
    let resumed = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        resumed_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(resumed_stream.clone()),
    )
    .expect("rebuild mobile engine after restart");

    resumed.runtime().block_on(async {
        resumed
            .submit(client::protocol::commands::ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .expect("restore the session containing the no-ID prompt");
        resumed
            .submit(client::protocol::commands::ClientCommand::ResumeTurn { turn_id })
            .await
            .expect("resume the internally assigned turn ID");
        let wait_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if !resumed_stream.captured_calls().await.is_empty() {
                        return Ok(());
                    }
                    if let Some(message) = resumed_listener.received.lock().await.iter().find_map(
                        |event| match event {
                            ClientEvent::Error { message, .. } => Some(message.display()),
                            _ => None,
                        },
                    ) {
                        return Err(message);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
        match wait_result {
            Ok(Ok(())) => {}
            Ok(Err(message)) => panic!("cold resume failed before the provider call: {message}"),
            Err(_) => panic!("cold resume did not reach the provider within five seconds"),
        }
        let calls = resumed_stream.captured_calls().await;
        let actual_images = calls[0]
            .messages
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { source } => Some(source.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual_images, expected_images);
        assert!(calls[0].messages.iter().any(|message| matches!(
            message,
            ConversationMessage::User { content, .. }
                if content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.encode_utf16().collect::<Vec<_>>() == prompt_units
                    }
                    ContentBlock::TextJsUtf16 {
                        text,
                        utf16_code_units,
                        ..
                    } => {
                        utf16_code_units == &prompt_units
                            && text.encode_utf16().collect::<Vec<_>>()
                                == String::from_utf16_lossy(utf16_code_units)
                                    .encode_utf16()
                                    .collect::<Vec<_>>()
                    }
                    _ => false,
                })
        )));
    });
}
#[test]
fn mobile_skill_slash_runs_and_cold_resumes_the_source_preserving_batch() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    use lingxi_core::types::{ContentBlock, ConversationMessage};
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };

    const PAUSE_INTERRUPTION: &str = "[Request interrupted by user]";

    struct SourceUserRow {
        id: lingxi_core::types::MessageId,
        text_utf16: Option<Vec<u16>>,
        is_meta: bool,
        turn_companion: Option<Option<bool>>,
    }

    fn source_user_rows(messages: &[ConversationMessage]) -> Vec<SourceUserRow> {
        messages
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::User {
                    id,
                    content,
                    is_meta,
                    turn_companion,
                    ..
                } => Some(SourceUserRow {
                    id: *id,
                    text_utf16: content.iter().find_map(|block| match block {
                        ContentBlock::Text { text, .. } => {
                            Some(text.encode_utf16().collect::<Vec<_>>())
                        }
                        ContentBlock::TextJsUtf16 {
                            utf16_code_units, ..
                        } => Some(utf16_code_units.clone()),
                        _ => None,
                    }),
                    is_meta: *is_meta,
                    turn_companion: *turn_companion,
                }),
                _ => None,
            })
            .collect()
    }

    fn source_user_images(
        messages: &[ConversationMessage],
    ) -> Vec<lingxi_core::types::ImageSource> {
        messages
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { source } => Some(source.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn bounded_utf16_diagnostic(units: &[u16]) -> String {
        use sha2::{Digest, Sha256};

        const PREFIX_UNITS: usize = 64;
        const SUFFIX_UNITS: usize = 16;
        let mut digest = Sha256::new();
        for unit in units {
            digest.update(unit.to_be_bytes());
        }
        let digest = digest.finalize();
        let digest_hex = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let escape = |slice: &[u16]| {
            slice
                .iter()
                .map(|unit| format!("\\u{unit:04x}"))
                .collect::<String>()
        };
        let escaped = if units.len() <= PREFIX_UNITS + SUFFIX_UNITS {
            escape(units)
        } else {
            format!(
                "{}...<{} UTF-16 units elided>...{}",
                escape(&units[..PREFIX_UNITS]),
                units.len() - PREFIX_UNITS - SUFFIX_UNITS,
                escape(&units[units.len() - SUFFIX_UNITS..]),
            )
        };
        format!(
            "units={}, sha256_utf16be={}, escaped={escaped:?}",
            units.len(),
            digest_hex,
        )
    }

    fn request_block_diagnostic(block: &ContentBlock) -> String {
        match block {
            ContentBlock::Text { text, .. } => format!(
                "text({})",
                bounded_utf16_diagnostic(&text.encode_utf16().collect::<Vec<_>>())
            ),
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => format!(
                "text_js_utf16({})",
                bounded_utf16_diagnostic(utf16_code_units)
            ),
            ContentBlock::Thinking { thinking, .. } => format!(
                "thinking({})",
                bounded_utf16_diagnostic(&thinking.encode_utf16().collect::<Vec<_>>())
            ),
            ContentBlock::ConnectorText { connector_text, .. } => format!(
                "connector_text({})",
                bounded_utf16_diagnostic(&connector_text.encode_utf16().collect::<Vec<_>>())
            ),
            ContentBlock::ProviderContent { protocol, .. } => {
                format!("provider_content(protocol={protocol:?}, payload=omitted)")
            }
            ContentBlock::ToolUse { name, .. } => {
                format!("tool_use(name={name:?}, payload=omitted)")
            }
            ContentBlock::ToolResult { .. } => "tool_result(payload=omitted)".to_string(),
            ContentBlock::Image { .. } => "image(payload=omitted)".to_string(),
            ContentBlock::Document { .. } => "document(payload=omitted)".to_string(),
            ContentBlock::RedactedThinking { .. } => {
                "redacted_thinking(payload=omitted)".to_string()
            }
            ContentBlock::ServerToolUse { name, .. } => {
                format!("server_tool_use(name={name:?}, payload=omitted)")
            }
            ContentBlock::AdvisorToolResult { .. } => {
                "advisor_tool_result(payload=omitted)".to_string()
            }
            ContentBlock::MediaAnalysis { .. } => "media_analysis(payload=omitted)".to_string(),
        }
    }

    fn bounded_request_rows_diagnostic(messages: &[ConversationMessage]) -> String {
        const MAX_MESSAGES: usize = 24;
        const MAX_BLOCKS_PER_USER: usize = 8;
        let rows = messages
            .iter()
            .take(MAX_MESSAGES)
            .enumerate()
            .map(|(message_index, message)| match message {
                ConversationMessage::User {
                    id,
                    content,
                    is_meta,
                    is_compact_summary,
                    is_visible_in_transcript_only,
                    turn_companion,
                    ..
                } => {
                    let blocks = content
                        .iter()
                        .take(MAX_BLOCKS_PER_USER)
                        .map(request_block_diagnostic)
                        .collect::<Vec<_>>();
                    format!(
                        "message[{message_index}] kind=user id={} is_meta={} is_compact_summary={} is_visible_in_transcript_only={} turn_companion={turn_companion:?} blocks={blocks:?} omitted_blocks={}",
                        id.as_uuid(),
                        is_meta,
                        is_compact_summary,
                        is_visible_in_transcript_only,
                        content.len().saturating_sub(MAX_BLOCKS_PER_USER),
                    )
                }
                ConversationMessage::Assistant { id, content, .. } => format!(
                    "message[{message_index}] kind=assistant id={} content_blocks={} payload=omitted",
                    id.as_uuid(),
                    content.len(),
                ),
                ConversationMessage::System { id, subtype, .. } => format!(
                    "message[{message_index}] kind=system id={} subtype={subtype:?} payload=omitted",
                    id.as_uuid(),
                ),
            })
            .collect::<Vec<_>>();
        format!(
            "first_prepared_request message_count={} rows=[{}] omitted_messages={}",
            messages.len(),
            rows.join("; "),
            messages.len().saturating_sub(MAX_MESSAGES),
        )
    }

    fn assert_skill_pair(user_rows: &[SourceUserRow], command_header_units: &[u16]) {
        assert_eq!(
            user_rows.len(),
            2,
            "each skill batch contains two source rows"
        );
        assert_eq!(
            user_rows[0]
                .text_utf16
                .as_deref()
                .expect("slash command keeps its exact UTF-16 projection"),
            command_header_units,
            "API request uses Native's serialized XML command header"
        );
        assert!(
            !user_rows[0].is_meta,
            "the command row is not synthetic metadata"
        );
        assert_eq!(
            user_rows[0].turn_companion, None,
            "the command row is not a companion"
        );
        let expected_body = "MOBILE SKILL BODY".encode_utf16().collect::<Vec<_>>();
        assert!(
            user_rows[1]
                .text_utf16
                .as_deref()
                .expect("expanded body keeps its exact UTF-16 projection")
                .windows(expected_body.len())
                .any(|window| window == expected_body.as_slice()),
            "expanded skill body remains a separate source row"
        );
        assert!(user_rows[1].is_meta, "expanded body is meta");
        assert_eq!(
            user_rows[1].turn_companion,
            Some(Some(true)),
            "expanded body carries Native turn-companion history metadata"
        );
    }

    fn contains_utf16(units: &[u16], text: &str) -> bool {
        let needle = text.encode_utf16().collect::<Vec<_>>();
        units
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
    }

    fn skill_listing_attachment_rows(transcript: &str) -> Vec<(String, Vec<u16>)> {
        session::jsonl::reader::route_lines(transcript)
            .messages_in_order
            .into_iter()
            .filter_map(|row| {
                if row.message_type != "attachment" {
                    return None;
                }
                let payload = row.attachment_projection?;
                if payload.string_at("/type") != Some("skill_listing") {
                    return None;
                }
                Some((row.uuid, payload.string_units("/names/0")?))
            })
            .collect()
    }

    fn transcript_typed_attachment_rows(
        transcript: &str,
    ) -> Vec<(String, String, Option<Utf16JsonProjection>)> {
        session::jsonl::reader::route_lines(transcript)
            .messages_in_order
            .into_iter()
            .filter(|row| row.message_type == "attachment")
            .map(|row| {
                let payload = row.attachment_projection;
                let kind = payload
                    .as_ref()
                    .and_then(|payload| payload.string_at("/type"))
                    .unwrap_or("<missing attachment type>")
                    .to_owned();
                (row.uuid, kind, payload)
            })
            .collect()
    }

    fn assert_transcript_attachments_are_packed_once_before_skill(
        phase: &str,
        transcript: &str,
        user_rows: &[SourceUserRow],
        command_index: usize,
    ) -> Vec<String> {
        let request_rows_brief = user_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                (
                    index,
                    row.id.as_uuid().to_string(),
                    row.is_meta,
                    row.turn_companion,
                    row.text_utf16.as_ref().map(Vec::len),
                )
            })
            .collect::<Vec<_>>();
        let mut request_ids = std::collections::HashSet::new();
        for row in user_rows {
            assert!(
                request_ids.insert(row.id.as_uuid().to_string()),
                "one source user row must not be duplicated in the request"
            );
        }

        let transcript_users = session::jsonl::reader::route_lines(transcript)
            .messages_in_order
            .into_iter()
            .filter(|row| row.message_type == "user")
            .collect::<Vec<_>>();
        let mut transcript_user_ids = std::collections::HashSet::new();
        let mut previous_request_user_index = None;
        for source_row in transcript_users {
            assert!(
                transcript_user_ids.insert(source_row.uuid.clone()),
                "a raw JSONL user-row UUID is unique: {}",
                source_row.uuid
            );
            let matching_request_rows = user_rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.id.as_uuid().to_string() == source_row.uuid)
                .collect::<Vec<_>>();
            assert_eq!(
                matching_request_rows.len(),
                1,
                "every raw JSONL user-row identity reaches the request once: {}",
                source_row.uuid
            );
            let (request_user_index, matching_request_row) = matching_request_rows[0];
            if let Some(previous_index) = previous_request_user_index {
                assert!(
                    previous_index < request_user_index,
                    "Native packing preserves the JSONL user-row sequence: {}",
                    source_row.uuid
                );
            }
            previous_request_user_index = Some(request_user_index);
            let source_text_units = source_row
                .message
                .string_units("/content/0/text")
                .expect("every source user row in this Skill batch keeps exact JSONL text units");
            assert_eq!(
                matching_request_row.text_utf16.as_deref(),
                Some(source_text_units.as_slice()),
                "the API row preserves every JSONL user's exact UTF-16 text: {}",
                source_row.uuid
            );
        }

        let attachments = transcript_typed_attachment_rows(transcript);
        let attachment_request_map = attachments
            .iter()
            .map(|(uuid, kind, _)| {
                let request_rows = user_rows
                    .iter()
                    .enumerate()
                    .filter_map(|(index, row)| {
                        (row.id.as_uuid().to_string() == *uuid).then_some(index)
                    })
                    .collect::<Vec<_>>();
                (kind.as_str(), uuid.as_str(), request_rows)
            })
            .collect::<Vec<_>>();
        let empty_session_context =
            Utf16JsonProjection::parse(r#"{"type":"session_context","context":{}}"#)
                .expect("the known empty durable session-context payload is valid JSON");
        let mut attachment_ids = std::collections::HashSet::new();
        let mut type_by_id = HashMap::new();
        let mut request_attachments = Vec::new();
        for (uuid, kind, payload) in &attachments {
            assert!(
                attachment_ids.insert(uuid.clone()),
                "phase={phase}: typed JSONL attachment UUID is unique: {uuid}"
            );
            let matches = user_rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.id.as_uuid().to_string() == *uuid)
                .collect::<Vec<_>>();
            if kind == "prompt_snapshot" {
                assert!(
                    matches.is_empty(),
                    "the durable prompt_snapshot sidecar is not a model request row"
                );
                assert_eq!(
                    transcript_uuid_count(transcript, uuid),
                    1,
                    "the non-request prompt_snapshot sidecar remains unique in JSONL"
                );
                continue;
            }
            let payload = payload
                .as_ref()
                .expect("each typed JSONL attachment retains its raw payload");
            if kind == "session_context" && payload == &empty_session_context {
                assert!(
                    lingxi_core::host::instruction_announcements::render_instruction_attachment(
                        payload
                    )
                    .is_none(),
                    "the exact empty session_context payload renders no Native model row"
                );
                assert!(
                    matches.is_empty(),
                    "the empty durable session_context stays absent from API user rows"
                );
                assert_eq!(
                    transcript_uuid_count(transcript, uuid),
                    1,
                    "the empty model-invisible session_context remains one durable JSONL sidecar"
                );
                continue;
            }
            request_attachments.push((uuid.clone(), kind.clone()));
            type_by_id.insert(uuid.clone(), kind.clone());
            assert_eq!(
                matches.len(),
                1,
                "phase={phase}: model-input JSONL attachment {kind} {uuid} reaches the request once; request rows (index,id,is_meta,turn_companion,text_utf16_units)={request_rows_brief:?}; JSONL attachment mapping (type,uuid,request_row_indexes)={attachment_request_map:?}"
            );
            let (index, row) = matches[0];
            assert!(
                index < command_index,
                "Native zjr packs typed attachment {kind} before the skill command"
            );
            assert!(row.is_meta, "typed attachment {kind} is a meta user row");
            assert_eq!(
                row.turn_companion, None,
                "typed attachment {kind} is not the Skill body companion"
            );
        }

        let request_type_order = user_rows[..command_index]
            .iter()
            .filter_map(|row| type_by_id.get(&row.id.as_uuid().to_string()).cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            request_type_order.len(),
            request_attachments.len(),
            "every request-eligible transcript attachment is classified; only the explicit prompt_snapshot sidecar is outside the model request"
        );
        let native_nar_types = request_type_order
            .iter()
            .filter(|kind| {
                matches!(
                    kind.as_str(),
                    "deferred_tools_delta"
                        | "environment"
                        | "model"
                        | "skill_listing"
                        | "auto_mode"
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            native_nar_types,
            [
                "environment",
                "model",
                "deferred_tools_delta",
                "skill_listing",
                "auto_mode",
            ],
            "the five NAr producers retain their source API order independently of date/session context"
        );
        let upe_positions = request_type_order
            .iter()
            .enumerate()
            .filter_map(|(index, kind)| (kind == "hook_additional_context").then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(
            upe_positions.len(),
            1,
            "the skill batch has one typed UserPromptExpansion attachment"
        );
        let last_nar_position = request_type_order
            .iter()
            .rposition(|kind| {
                matches!(
                    kind.as_str(),
                    "deferred_tools_delta"
                        | "environment"
                        | "model"
                        | "skill_listing"
                        | "auto_mode"
                )
            })
            .expect("all five Native NAr attachments are present");
        assert!(
            upe_positions[0] > last_nar_position,
            "Native packs Qe's single UPE attachment after every NAr attachment"
        );
        request_type_order
    }

    fn assert_known_native_skill_attachment_order(types: &[String]) {
        let expected_nar = [
            "environment",
            "model",
            "deferred_tools_delta",
            "skill_listing",
            "auto_mode",
        ];
        let actual_nar = types
            .iter()
            .filter(|kind| {
                matches!(
                    kind.as_str(),
                    "deferred_tools_delta"
                        | "environment"
                        | "model"
                        | "skill_listing"
                        | "auto_mode"
                )
            })
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            actual_nar,
            expected_nar.as_slice(),
            "the strict NAr subsequence follows Native source order"
        );
        let upe_positions = types
            .iter()
            .enumerate()
            .filter_map(|(index, kind)| (kind == "hook_additional_context").then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(upe_positions.len(), 1, "exactly one UPE row is present");
        let last_nar_position = types
            .iter()
            .rposition(|kind| expected_nar.contains(&kind.as_str()))
            .expect("five NAr rows are present");
        assert!(
            upe_positions[0] > last_nar_position,
            "UPE follows all NAr rows; date and session_context are checked independently"
        );
        assert!(
            types.iter().all(|kind| {
                expected_nar.contains(&kind.as_str())
                    || matches!(
                        kind.as_str(),
                        "date" | "session_context" | "hook_additional_context"
                    )
            }),
            "every typed attachment is classified as one of five NAr producers, date/session_context, or UPE"
        );
        assert!(
            types.iter().filter(|kind| kind.as_str() == "date").count() <= 1,
            "the optional date row is handled separately from NAr order"
        );
    }

    fn user_prompt_expansion_context_rows(transcript: &str) -> Vec<(String, Vec<u16>)> {
        session::jsonl::reader::route_lines(transcript)
            .messages_in_order
            .into_iter()
            .filter_map(|row| {
                if row.message_type != "attachment" {
                    return None;
                }
                let payload = row.attachment_projection?;
                if payload.string_at("/type") != Some("hook_additional_context")
                    || payload.string_at("/hookEvent") != Some("UserPromptExpansion")
                {
                    return None;
                }
                Some((row.uuid, payload.string_units("/content/0")?))
            })
            .collect()
    }

    fn transcript_user_text_units(transcript: &str, uuid: &str) -> Option<Vec<u16>> {
        session::jsonl::reader::route_lines(transcript)
            .messages_in_order
            .into_iter()
            .find(|row| row.message_type == "user" && row.uuid == uuid)
            .and_then(|row| row.message.string_units("/content/0/text"))
    }

    fn transcript_uuid_count(transcript: &str, uuid: &str) -> usize {
        transcript
            .lines()
            .filter_map(|line| {
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line).ok()
            })
            .filter(|row| row.string_at("/uuid") == Some(uuid))
            .count()
    }

    fn install_user_prompt_expansion_hook(root: &std::path::Path) {
        let hooks_dir = root.join(".lingxi");
        std::fs::create_dir_all(&hooks_dir).expect("create mobile hook settings directory");
        let settings = serde_json::json!({
            "hooks": {
                "UserPromptExpansion": [{
                    "hooks": [{ "type": "command", "command": "capture-upe-hook" }]
                }]
            }
        });
        std::fs::write(
            hooks_dir.join("settings.json"),
            serde_json::to_vec(&settings).expect("serialize mobile hook settings"),
        )
        .expect("write UserPromptExpansion hook settings");
    }

    #[derive(Clone, Debug)]
    struct CapturedHookProcessCommand {
        command: String,
        args: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        env: HashMap<String, String>,
        timeout: Option<std::time::Duration>,
        stdin: Option<String>,
    }

    #[derive(Default)]
    struct CapturingUserPromptExpansionProcess {
        commands: StdMutex<Vec<CapturedHookProcessCommand>>,
    }

    impl CapturingUserPromptExpansionProcess {
        fn captured_commands(&self) -> Vec<CapturedHookProcessCommand> {
            self.commands
                .lock()
                .expect("lock captured hook calls")
                .clone()
        }
    }

    #[async_trait]
    impl lingxi_core::host::ProcessRunner for CapturingUserPromptExpansionProcess {
        async fn run(
            &self,
            command: &lingxi_core::host::SandboxedCommand,
        ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError> {
            let command = command.inner();
            self.commands
                .lock()
                .expect("lock captured hook calls")
                .push(CapturedHookProcessCommand {
                    command: command.command.clone(),
                    args: command.args.clone(),
                    cwd: command.cwd.clone(),
                    env: command.env.clone(),
                    timeout: command.timeout,
                    stdin: command.stdin.clone(),
                });
            Ok(mobile_linux_api::ProcessOutput {
                stdout: r#"{"hookSpecificOutput":{"hookEventName":"UserPromptExpansion","additionalContext":"UPE \ud800"}}"#.into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }

        async fn spawn_background(
            &self,
            _command: &lingxi_core::host::SandboxedCommand,
        ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError> {
            Err(mobile_linux_api::ProcessError::Unsupported)
        }

        async fn kill(
            &self,
            _handle: &lingxi_core::host::ProcessHandle,
        ) -> Result<(), mobile_linux_api::ProcessError> {
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    fn assert_user_prompt_expansion_wire_input(
        command: &CapturedHookProcessCommand,
        cwd: &Path,
        session_id: &str,
        transcript_path: &Path,
        permission_mode: &str,
        command_units: &[u16],
    ) {
        assert_eq!(command.command, "/bin/sh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            ["-c", "capture-upe-hook"]
        );
        assert_eq!(command.cwd.as_deref(), Some(cwd));
        assert_eq!(
            command.timeout,
            Some(std::time::Duration::from_millis(
                hooks::HOOK_COMMAND_TIMEOUT_MS
            ))
        );
        assert_eq!(command.env.get("LINGXI").map(String::as_str), Some("1"));
        assert_eq!(
            command.env.get("LINGXI_CHILD_SESSION").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            command.env.get("LINGXI_SESSION_ID").map(String::as_str),
            Some(session_id)
        );
        assert_eq!(
            command
                .env
                .get(branding::PROJECT_DIR_ENV)
                .map(String::as_str),
            Some(cwd.to_string_lossy().as_ref())
        );

        let stdin = command.stdin.as_deref().expect("hook receives JSON stdin");
        let body = stdin
            .strip_suffix('\n')
            .expect("Native command hook input is terminated by LF");
        assert!(
            body.contains(r#"\ud800"#),
            "wire JSON keeps the escaped lone surrogate"
        );
        let payload = Utf16JsonProjection::parse(body).expect("hook input is valid JSON");
        assert_eq!(
            payload.string_at("/hook_event_name"),
            Some("UserPromptExpansion")
        );
        assert_eq!(payload.string_at("/expansion_type"), Some("slash_command"));
        assert_eq!(payload.string_at("/command_source"), Some("user"));
        assert_eq!(payload.string_at("/session_id"), Some(session_id));
        assert_eq!(
            payload.string_at("/transcript_path"),
            Some(transcript_path.to_string_lossy().as_ref())
        );
        assert_eq!(
            payload.string_at("/cwd"),
            Some(cwd.to_string_lossy().as_ref())
        );
        assert_eq!(payload.string_at("/permission_mode"), Some(permission_mode));
        let expected_name = "mobile-review".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            payload.string_units("/command_name").as_deref(),
            Some(expected_name.as_slice())
        );
        assert_eq!(
            payload.string_units("/command_args").as_deref(),
            Some(&[0xd800][..]),
            "the argument projection reaches command stdin without lossy String conversion"
        );
        assert_eq!(
            payload.string_units("/prompt").as_deref(),
            Some(command_units),
            "the complete slash prompt preserves the original UTF-16 code units"
        );
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    install_user_prompt_expansion_hook(tmp.path());
    write_skill(
        tmp.path(),
        "mobile-review",
        "Mobile review fixture",
        "MOBILE SKILL BODY",
    );
    let command_units = "/mobile-review "
        .encode_utf16()
        .chain([0xd800])
        .collect::<Vec<_>>();
    let command_images = vec![client::protocol::commands::ImageRefDto {
        media_type: "image/png".into(),
        base64: "cG5nLWltYWdl".into(),
    }];
    let expected_command_images = vec![lingxi_core::types::ImageSource::Base64 {
        media_type: "image/png".into(),
        data: "cG5nLWltYWdl".into(),
    }];
    let command_header_units = "<command-message>mobile-review</command-message>\n<command-name>/mobile-review</command-name>\n<command-args>"
        .encode_utf16()
        .chain([0xd800])
        .chain("</command-args>".encode_utf16())
        .collect::<Vec<_>>();
    assert_eq!(
        command_header_units.len(),
        123,
        "the native API command header includes its XML wrapper and lone surrogate"
    );
    assert_ne!(
        command_header_units, command_units,
        "hook input remains the raw slash command while API history carries the XML header"
    );
    assert_ne!(
        orchestrator::prompt::total_tokens::resolve_mode(None),
        orchestrator::prompt::total_tokens::TotalTokensMode::Off,
        "the regression requires the token reminder feature to be enabled by the process environment or default"
    );
    assert!(
        orchestrator::prompt::total_tokens::after_user_turn(None),
        "the regression requires the after-user token flag to be enabled"
    );
    let expected_upe_context = "UPE ".encode_utf16().chain([0xd800]).collect::<Vec<_>>();
    let (entered_tx, mut entered_rx) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let hook_process = Arc::new(CapturingUserPromptExpansionProcess::default());
    let first_listener = Arc::new(FakeListener::default());
    let first = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(
            HostFakePlatform::new(tmp.path().to_path_buf())
                .with_process_runner(hook_process.clone()),
        ),
        first_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(pending.clone()),
    )
    .expect("build mobile host with a pending stream");

    let first_for_work = first.clone();
    let first_command_units = command_units.clone();
    let first_command_images = command_images.clone();
    let first_expected_command_images = expected_command_images.clone();
    let first_command_header_units = command_header_units.clone();
    let first_expected_upe_context = expected_upe_context.clone();
    let first_hook_process = hook_process.clone();
    let first_session =
        first.runtime().block_on(async move {
            let first = first_for_work;
            let session_id = first
                .inner()
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string();
            first
                .inner()
                .session_writer
                .append_mobile_empty_session(&session_id, "skill cold replay", &first.session_cwd())
                .await
                .expect("anchor the session before restart");
            first
                .inner()
                .session_writer
                .append_session_mode(first.inner().session_mode.as_str())
                .await
                .expect("persist session mode");

            first
                .submit(ClientCommand::RunSlashCommand {
                    raw: ClientTextDto::from_code_units(first_command_units.clone()),
                    images: first_command_images.clone(),
                    turn_id: Some(247),
                })
                .await
                .expect("dispatch mobile skill as a turn batch");
            let wait_result =
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let stream_entry_closed = match entered_rx.try_recv() {
                            Ok(()) => return Ok(()),
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => true,
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
                        };
                        if let Some(message) = first_listener.received.lock().await.iter().find_map(
                            |event| match event {
                                ClientEvent::Error { message, .. } => Some(message.display()),
                                _ => None,
                            },
                        ) {
                            return Err(message);
                        }
                        if stream_entry_closed {
                            return Err("stream entry signal closed".to_string());
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await;
            match wait_result {
                Ok(Ok(())) => {}
                Ok(Err(message)) => {
                    panic!("initial skill prompt failed before model call: {message}");
                }
                Err(_) => {
                    let errors = first_listener
                        .received
                        .lock()
                        .await
                        .iter()
                        .filter_map(|event| match event {
                            ClientEvent::Error { message, .. } => Some(message.display()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    panic!("skill batch reaches the streaming client; listener errors: {errors:?}");
                }
            }

            let first_calls = pending.captured_calls();
            assert_eq!(first_calls.len(), 1);
            assert_eq!(
                source_user_images(&first_calls[0].messages),
                first_expected_command_images,
                "RunSlashCommand image bytes reach the first skill model request"
            );
            let hook_commands = first_hook_process.captured_commands();
            assert_eq!(
                hook_commands.len(),
                1,
                "the UPE command reaches ProcessRunner"
            );
            let first_user_rows = source_user_rows(&first_calls[0].messages);
            assert_eq!(
                first_user_rows.len(),
                9,
                "warm Skill request has seven ordered prefix rows followed by command/body; bounded rows:\n{}",
                bounded_request_rows_diagnostic(&first_calls[0].messages),
            );
            let command_row_index = first_user_rows
                .iter()
                .position(|row| row.text_utf16.as_deref() == Some(first_command_header_units.as_slice()))
                .expect("Native API projection retains the exact slash command");
            assert_eq!(command_row_index, 7, "Native Qe attachment rows precede the command");
            let body_row_index = command_row_index + 1;
            assert_eq!(body_row_index + 1, first_user_rows.len());
            assert_skill_pair(
                &first_user_rows[command_row_index..],
                &first_command_header_units,
            );

            let expected_prefix_markers = [
                "Today's date is ",
                "# Environment",
                "You are powered by the model",
                "The following deferred tools",
                "The following skills are available for use with the Skill tool:",
            ];
            for (index, marker) in expected_prefix_markers.iter().enumerate() {
                let row = &first_user_rows[index];
                assert!(row.is_meta, "context prefix row {index} is metadata");
                assert_eq!(
                    row.turn_companion, None,
                    "context prefix row {index} is not the Skill body companion"
                );
                assert!(
                    contains_utf16(
                        row.text_utf16.as_deref().expect("prefix row keeps UTF-16 text"),
                        marker
                    ),
                    "prefix row {index} contains {marker:?}: {}",
                    bounded_request_rows_diagnostic(&first_calls[0].messages)
                );
            }
            assert!(
                !first_user_rows.iter().any(|row| {
                    row.text_utf16
                        .as_deref()
                        .is_some_and(|units| contains_utf16(units, "<total_tokens>"))
                }),
                "LocalSkill does not stage total_tokens even with after-user enabled"
            );
            let system_markers = first_calls[0]
                .messages
                .iter()
                .enumerate()
                .filter_map(|(index, message)| match message {
                    ConversationMessage::System { subtype, .. }
                        if subtype.as_deref() == Some("model_reminder_attachment") =>
                    {
                        Some(index)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                system_markers.len(),
                1,
                "the request keeps its model-reminder marker"
            );
            let date_user_row_index = 0;
            let date_message_index = first_calls[0]
                .messages
                .iter()
                .position(|message| message.id() == first_user_rows[date_user_row_index].id)
                .expect("date metadata remains in the outgoing request");
            assert_eq!(
                system_markers[0] + 1,
                date_message_index,
                "the model-reminder marker stays immediately before the date context"
            );

            let listing_row_index = 4;
            let upe_row_index = first_user_rows
                .iter()
                .position(|row| {
                    row.text_utf16.as_deref().is_some_and(|units| {
                        contains_utf16(units, "UserPromptExpansion hook additional context: UPE ")
                    })
                })
                .expect("the current UPE row remains model-visible");
            let auto_mode_row_index = first_user_rows
                .iter()
                .position(|row| {
                    row.text_utf16.as_deref().is_some_and(|units| {
                        contains_utf16(units, "Bias toward working without stopping for clarifying questions")
                            || contains_utf16(units, "While auto mode is active:")
                            || contains_utf16(units, "While bypass permissions mode is active:")
                    })
                })
                .expect("the current auto_mode attachment is model-visible");
            assert!(
                listing_row_index < auto_mode_row_index
                    && date_user_row_index < upe_row_index
                    && auto_mode_row_index < upe_row_index
                    && upe_row_index < command_row_index,
                "Native NAr auto_mode follows skill_listing and precedes Qe's UPE and the slash command"
            );
            assert!(first_user_rows[auto_mode_row_index].is_meta);
            assert_eq!(first_user_rows[auto_mode_row_index].turn_companion, None);
            let upe_message_id = first_user_rows[upe_row_index].id;
            let upe_text_units = first_user_rows[upe_row_index]
                .text_utf16
                .as_ref()
                .expect("UPE context keeps its UTF-16 text projection")
                .clone();
            let skill_listing_uuid =
                first_user_rows[listing_row_index].id.as_uuid().to_string();
            let command_message_id = first_user_rows[command_row_index].id;
            let body_message_id = first_user_rows[body_row_index].id;
            let skill_listing_text_units = first_user_rows[listing_row_index]
                .text_utf16
                .as_ref()
                .expect("skill listing keeps its UTF-16 text projection")
                .clone();
            assert!(
                first_user_rows[upe_row_index]
                    .text_utf16
                    .as_deref()
                    .expect("UPE context keeps its UTF-16 text projection")
                    .windows(first_expected_upe_context.len())
                    .any(|window| window == first_expected_upe_context.as_slice()),
                "the warm request preserves the UPE source code units"
            );
            assert!(contains_utf16(
                first_user_rows[listing_row_index]
                    .text_utf16
                    .as_deref()
                    .expect("skill listing keeps its UTF-16 text projection"),
                "mobile-review"
            ));
            let transcript_path =
                session::jsonl::session_path(&first.lingxi_home, &first.session_cwd(), &session_id);
            assert_user_prompt_expansion_wire_input(
                &hook_commands[0],
                std::path::Path::new(&first.session_cwd()),
                &session_id,
                &transcript_path,
                &first
                    .inner()
                    .orchestrator
                    .permission_mode()
                    .unwrap_or_else(|| "default".to_string()),
                &first_command_units,
            );
            let transcript = std::fs::read_to_string(transcript_path)
                .expect("read durable skill transcript rows");
            let warm_attachment_types = assert_transcript_attachments_are_packed_once_before_skill(
                "warm",
                &transcript,
                &first_user_rows,
                command_row_index,
            );
            assert_known_native_skill_attachment_order(&warm_attachment_types);
            let command_row = transcript
                .find("<command-message>mobile-review</command-message>")
                .expect("Native XML command header reached JSONL");
            assert_eq!(
                transcript_user_text_units(&transcript, &command_message_id.as_uuid().to_string())
                    .as_deref(),
                Some(first_command_header_units.as_slice()),
                "the JSONL command row preserves Native's exact 123 UTF-16 header units"
            );
            let body_row = transcript
                .find("MOBILE SKILL BODY")
                .expect("expanded body reached JSONL as its own row");
            let upe_uuid = upe_message_id.as_uuid().to_string();
            let upe_row = transcript
                .find(&upe_uuid)
                .expect("raw UserPromptExpansion row reached JSONL");
            let listing_row = transcript
                .find(&skill_listing_uuid)
                .expect("skill listing attachment reached JSONL");
            assert!(
                command_row < body_row && body_row < listing_row && listing_row < upe_row,
                "H durable ingress byte offsets (command/body/listing/UPE) are {command_row}/{body_row}/{listing_row}/{upe_row}; expected command < body < listing < UPE"
            );
            assert!(transcript[body_row..].contains("\"turnCompanion\":true"));
            assert_eq!(
                skill_listing_attachment_rows(&transcript),
                vec![(
                    skill_listing_uuid.clone(),
                    "mobile-review".encode_utf16().collect(),
                )],
                "the fourth request row is backed by one typed skill_listing JSONL attachment"
            );
            assert_eq!(
                user_prompt_expansion_context_rows(&transcript),
                vec![(upe_uuid.clone(), first_expected_upe_context.clone())],
                "the deferred raw UPE attachment retains exact UTF-16 source units"
            );
            assert_eq!(
                transcript_uuid_count(&transcript, &upe_uuid),
                1,
                "the UPE attachment UUID is durable exactly once"
            );

            let checkpoint = first
                .durable_turns
                .load(&session_id, 247)
                .expect("load skill turn checkpoint");
            assert_eq!(
                checkpoint.images,
                first_command_images,
                "the accepted slash image is retained by the durable checkpoint"
            );
            let source = checkpoint
                .skill_prompt
                .as_ref()
                .expect("checkpoint retains source inputs for the complete batch");
            assert_eq!(source.command.string_units(""), Some(first_command_header_units));
            assert!(source_string_contains(&source.body, "MOBILE SKILL BODY"));
            let source_body_units = source
                .body
                .string_units("")
                .expect("checkpoint retains exact skill body source units");
            assert_eq!(
                first_user_rows[body_row_index].text_utf16.as_deref(),
                Some(source_body_units.as_slice()),
                "the outgoing body row matches the durable skill source projection exactly"
            );
            assert_eq!(source.hook_messages.len(), 1);
            assert_eq!(source.hook_message_ids, vec![upe_message_id]);
            assert_eq!(
                source.hook_messages[0].string_at("/type"),
                Some("hook_additional_context")
            );
            assert_eq!(
                source.hook_messages[0].string_at("/hookEvent"),
                Some("UserPromptExpansion")
            );
            assert_eq!(
                source.hook_messages[0]
                    .string_units("/content/0")
                    .as_deref(),
                Some(first_expected_upe_context.as_slice()),
                "checkpoint retains the hook's exact source projection"
            );

            first
                .submit(ClientCommand::PauseTurn {
                    turn_id: 247,
                    reason: "simulated_process_restart".into(),
                })
                .await
                .expect("quiesce the still-pending skill batch before restart");
            let paused = first
                .durable_turns
                .load(&session_id, 247)
                .expect("load the quiesced skill checkpoint");
            assert_eq!(paused.state, Some(TurnRecoveryStateDto::PausedRecoverable));
            assert_eq!(paused.queue_delivery, QueueDeliveryState::Claimed);
            assert!(paused.safe_to_resume);
            let paused_transcript = std::fs::read_to_string(session::jsonl::session_path(
                &first.lingxi_home,
                &first.session_cwd(),
                &session_id,
            ))
            .expect("read the quiesced skill transcript");
            let interruption_ids = session::jsonl::reader::route_lines(&paused_transcript)
                .messages_in_order
                .into_iter()
                .filter(|row| {
                    row.message_type == "user"
                        && row.message.string_at("/content/0/text") == Some(PAUSE_INTERRUPTION)
                })
                .map(|row| row.uuid)
                .collect::<Vec<_>>();
            assert_eq!(
                interruption_ids.len(),
                1,
                "quiescing the pending stream persists exactly one interruption row"
            );
            let interruption_uuid = interruption_ids[0].clone();
            assert_eq!(transcript_uuid_count(&paused_transcript, &interruption_uuid), 1);
            (
                session_id,
                skill_listing_uuid,
                skill_listing_text_units,
                command_message_id,
                body_message_id,
                upe_message_id,
                upe_text_units,
                interruption_uuid,
            )
        });
    let (
        session_id,
        skill_listing_uuid,
        skill_listing_text_units,
        command_message_id,
        body_message_id,
        upe_message_id,
        upe_text_units,
        interruption_uuid,
    ) = first_session;
    drop(first);

    let resumed_stream = Arc::new(MockStreamingApiClient::with_turns(vec![
        vec![
            message_start("mobile-skill-replay", "test"),
            content_block_start_text(0),
            text_delta(0, "resumed"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
        vec![
            message_start("mobile-skill-new-send", "test"),
            content_block_start_text(0),
            text_delta(0, "second send"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
    ]));
    let resumed_listener = Arc::new(FakeListener::default());
    let resumed = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(
            HostFakePlatform::new(tmp.path().to_path_buf())
                .with_process_runner(hook_process.clone()),
        ),
        resumed_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(resumed_stream.clone()),
    )
    .expect("rebuild mobile host after restart");

    resumed.runtime().block_on(async {
        resumed
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .expect("restore the session containing the skill turn");
        let paused = resumed
            .durable_turns
            .load(&session_id, 247)
            .expect("reload the skill checkpoint after restart");
        assert_eq!(paused.state, Some(TurnRecoveryStateDto::PausedRecoverable));
        assert_eq!(paused.queue_delivery, QueueDeliveryState::Claimed);
        assert!(paused.safe_to_resume);
        resumed
            .submit(ClientCommand::ResumeTurn { turn_id: 247 })
            .await
            .expect("resume the whole stored skill batch");
        let wait_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if !resumed_stream.captured_calls().await.is_empty() {
                        return None;
                    }
                    if let Some(message) = resumed_listener.received.lock().await.iter().find_map(
                        |event| match event {
                            ClientEvent::Error { message, .. } => Some(message.display()),
                            _ => None,
                        },
                    ) {
                        return Some(message);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
        match wait_result {
            Ok(Some(message)) => panic!("cold resume failed before model call: {message}"),
            Ok(None) => {}
            Err(_) => {
                let errors = resumed_listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .filter_map(|event| match event {
                        ClientEvent::Error { message, .. } => Some(message.display()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                panic!("cold resume reaches the model request; listener errors: {errors:?}");
            }
        }

        let calls = resumed_stream.captured_calls().await;
        assert_eq!(calls.len(), 1);
        let hook_commands = hook_process.captured_commands();
        assert_eq!(hook_commands.len(), 1, "ResumeTurn must not rerun UPE");
        let resumed_user_rows = source_user_rows(&calls[0].messages);
        let resumed_command_index = resumed_user_rows
            .iter()
            .position(|row| row.id == command_message_id)
            .expect("cold request contains the original command identity");
        let resumed_body_index = resumed_command_index + 1;
        assert_eq!(
            resumed_body_index + 2,
            resumed_user_rows.len(),
            "cold request ends with the original command/body pair and the pause interruption; bounded rows:\n{}",
            bounded_request_rows_diagnostic(&calls[0].messages),
        );
        for (index, row) in resumed_user_rows[..resumed_command_index]
            .iter()
            .enumerate()
        {
            assert!(row.is_meta, "cold prefix row {index} is metadata");
            assert_eq!(
                row.turn_companion, None,
                "cold prefix row {index} is not a Skill body companion"
            );
        }
        assert_skill_pair(
            &resumed_user_rows[resumed_command_index..=resumed_body_index],
            &command_header_units,
        );
        assert_eq!(resumed_user_rows[resumed_body_index].id, body_message_id);
        let interruption = &resumed_user_rows[resumed_body_index + 1];
        let interruption_units = PAUSE_INTERRUPTION.encode_utf16().collect::<Vec<_>>();
        assert_eq!(interruption.id.as_uuid().to_string(), interruption_uuid);
        assert_ne!(interruption.id, command_message_id);
        assert_ne!(interruption.id, body_message_id);
        assert!(!interruption.is_meta);
        assert_eq!(interruption.turn_companion, None);
        assert_eq!(
            interruption.text_utf16.as_deref(),
            Some(interruption_units.as_slice()),
            "the pause interruption keeps its exact source text"
        );
        assert_eq!(
            resumed_user_rows
                .iter()
                .filter(|row| row.text_utf16.as_deref() == Some(interruption_units.as_slice()))
                .count(),
            1,
            "the pause interruption appears exactly once in the cold request"
        );
        let resumed_upe_index = resumed_user_rows
            .iter()
            .position(|row| row.id == upe_message_id)
            .expect("cold request contains the original UPE identity");
        assert!(resumed_upe_index < resumed_command_index);
        assert!(resumed_user_rows[resumed_upe_index].is_meta);
        assert_eq!(resumed_user_rows[resumed_upe_index].turn_companion, None);
        assert_eq!(
            resumed_user_rows[resumed_upe_index].text_utf16.as_deref(),
            Some(upe_text_units.as_slice()),
            "cold resume restores the exact UTF-16 rendered hook context"
        );
        let resumed_listing_index = resumed_user_rows
            .iter()
            .position(|row| row.id.as_uuid().to_string() == skill_listing_uuid)
            .expect("cold request contains the inherited listing identity");
        assert!(resumed_listing_index < resumed_command_index);
        assert!(resumed_user_rows[resumed_listing_index].is_meta);
        assert_eq!(
            resumed_user_rows[resumed_listing_index].turn_companion,
            None
        );
        assert_eq!(
            resumed_user_rows[resumed_listing_index]
                .text_utf16
                .as_deref(),
            Some(skill_listing_text_units.as_slice()),
            "cold restore preserves the original listing text source units"
        );
        assert!(
            !resumed_user_rows.iter().any(|row| {
                row.text_utf16
                    .as_deref()
                    .is_some_and(|units| contains_utf16(units, "<total_tokens>"))
            }),
            "cold LocalSkill resume also suppresses total_tokens"
        );
        let resumed_transcript_path =
            session::jsonl::session_path(&resumed.lingxi_home, &resumed.session_cwd(), &session_id);
        let resumed_transcript = std::fs::read_to_string(resumed_transcript_path)
            .expect("read resumed skill transcript rows");
        assert_eq!(
            transcript_user_text_units(
                &resumed_transcript,
                &command_message_id.as_uuid().to_string()
            )
            .as_deref(),
            Some(command_header_units.as_slice()),
            "cold resume preserves the exact Native API command-header projection"
        );
        let _ = assert_transcript_attachments_are_packed_once_before_skill(
            "cold",
            &resumed_transcript,
            &resumed_user_rows,
            resumed_command_index,
        );
        assert_eq!(
            skill_listing_attachment_rows(&resumed_transcript),
            vec![(skill_listing_uuid, "mobile-review".encode_utf16().collect(),)],
            "restoring the typed sent-skill set prevents a duplicate skill_listing attachment"
        );
        assert_eq!(
            user_prompt_expansion_context_rows(&resumed_transcript),
            vec![(
                upe_message_id.as_uuid().to_string(),
                expected_upe_context.clone()
            )],
            "cold retry leaves one raw UPE attachment with its original source identity"
        );
        assert_eq!(
            transcript_uuid_count(&resumed_transcript, &upe_message_id.as_uuid().to_string()),
            1
        );
        assert_eq!(
            transcript_uuid_count(
                &resumed_transcript,
                &command_message_id.as_uuid().to_string()
            ),
            1
        );
        assert_eq!(
            transcript_uuid_count(&resumed_transcript, &body_message_id.as_uuid().to_string()),
            1
        );
        let checkpoint = resumed
            .durable_turns
            .load(&session_id, 247)
            .expect("read resumed skill checkpoint");
        assert_eq!(
            checkpoint.images, command_images,
            "cold recovery retains the original slash image attachment"
        );
        assert_eq!(
            checkpoint
                .skill_prompt
                .as_ref()
                .expect("skill envelope remains attached to the turn")
                .command
                .string_units(""),
            Some(command_header_units.clone())
        );
        let source_body_units = checkpoint
            .skill_prompt
            .as_ref()
            .expect("skill envelope remains attached to the turn")
            .body
            .string_units("")
            .expect("resumed checkpoint keeps exact skill body source units");
        assert_eq!(
            resumed_user_rows[resumed_body_index].text_utf16.as_deref(),
            Some(source_body_units.as_slice()),
            "the reused body row matches the durable skill source projection exactly"
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if resumed.active_cancel.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cold resumed turn releases its slot before a new send");
        resumed
            .submit(ClientCommand::RunSlashCommand {
                raw: ClientTextDto::from_code_units(command_units.clone()),
                images: Vec::new(),
                turn_id: Some(248),
            })
            .await
            .expect("submit the same skill source as a new turn");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if resumed_stream.captured_calls().await.len() >= 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("new same-text skill turn reaches the streaming client");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if resumed.active_cancel.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("new same-text skill turn completes its durable writes");
        let calls = resumed_stream.captured_calls().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(
            source_user_images(&calls[0].messages),
            expected_command_images,
            "cold ResumeTurn reconstructs RunSlashCommand images from durable checkpoint"
        );
        let hook_commands = hook_process.captured_commands();
        assert_eq!(hook_commands.len(), 2, "a new turn fires UPE once");
        let transcript_path =
            session::jsonl::session_path(&resumed.lingxi_home, &resumed.session_cwd(), &session_id);
        assert_user_prompt_expansion_wire_input(
            &hook_commands[1],
            std::path::Path::new(&resumed.session_cwd()),
            &session_id,
            &transcript_path,
            &resumed
                .inner()
                .orchestrator
                .permission_mode()
                .unwrap_or_else(|| "default".to_string()),
            &command_units,
        );
        let second_user_rows = source_user_rows(&calls[1].messages);
        let command_rows = second_user_rows
            .iter()
            .filter(|row| row.text_utf16.as_deref() == Some(command_header_units.as_slice()))
            .collect::<Vec<_>>();
        assert_eq!(
            command_rows.len(),
            2,
            "same text is a new turn, not a replay"
        );
        assert_ne!(command_rows[0].id, command_rows[1].id);
        assert_ne!(command_rows[1].id, command_message_id);
        let resumed_transcript = std::fs::read_to_string(&transcript_path)
            .expect("read transcript after the new same-text turn");
        for row in &command_rows {
            assert_eq!(
                transcript_user_text_units(&resumed_transcript, &row.id.as_uuid().to_string())
                    .as_deref(),
                Some(command_header_units.as_slice()),
                "each same-text send keeps the Native XML command header in JSONL"
            );
        }
        let expected_body = "MOBILE SKILL BODY".encode_utf16().collect::<Vec<_>>();
        let body_rows = second_user_rows
            .iter()
            .filter(|row| {
                row.text_utf16.as_deref().is_some_and(|units| {
                    units
                        .windows(expected_body.len())
                        .any(|window| window == expected_body.as_slice())
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(body_rows.len(), 2);
        assert_ne!(body_rows[0].id, body_rows[1].id);
        assert_ne!(body_rows[1].id, body_message_id);
        let second_old_upe_row = second_user_rows
            .iter()
            .find(|row| row.id == upe_message_id)
            .expect("the replayed UPE row remains in the next request by identity");
        assert_eq!(
            second_old_upe_row.text_utf16.as_deref(),
            Some(upe_text_units.as_slice())
        );
        let second_upe_rows = second_user_rows
            .iter()
            .filter(|row| {
                row.text_utf16.as_deref().is_some_and(|units| {
                    units
                        .windows(expected_upe_context.len())
                        .any(|window| window == expected_upe_context.as_slice())
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            second_upe_rows.len(),
            2,
            "the next request carries the old and new same-text UPE rows"
        );
        let second_upe_row = second_upe_rows
            .iter()
            .find(|row| row.id != upe_message_id)
            .copied()
            .expect("new same-text send runs the configured UPE hook");
        assert_eq!(
            second_upe_row.text_utf16.as_deref(),
            Some(upe_text_units.as_slice())
        );
        assert_ne!(second_upe_row.id, upe_message_id);
        assert_eq!(
            transcript_uuid_count(
                &resumed_transcript,
                &command_rows[1].id.as_uuid().to_string()
            ),
            1,
            "new source turn gets one distinct durable UUID instead of text deduplication"
        );
        assert_eq!(
            user_prompt_expansion_context_rows(&resumed_transcript)
                .into_iter()
                .map(|(uuid, _)| uuid)
                .collect::<Vec<_>>(),
            vec![
                upe_message_id.as_uuid().to_string(),
                second_upe_row.id.as_uuid().to_string(),
            ],
            "a new same-text send appends a fresh UPE row instead of deduplicating by text"
        );
        assert_eq!(
            transcript_uuid_count(&resumed_transcript, &body_rows[1].id.as_uuid().to_string()),
            1
        );
    });
}

/// `submit(Cancel)` does not return at token-fire time: it waits until the
/// owned turn has unwound and released the slot, so an immediate session
/// transition cannot race the cancelled task.
#[test]
fn submit_cancel_waits_for_cleanup_before_new_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        // Arm a turn so a cancel token is in flight.
        handle
            .submit(ClientCommand::SendPrompt {
                text: "drive a turn".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: None,
                visualization_context: None,
            })
            .await
            .expect("submit(SendPrompt) ok");

        assert!(
            !handle.active_turn_is_cancelled().await,
            "the freshly-armed turn token must not be cancelled yet"
        );

        handle
            .submit(ClientCommand::Cancel { turn_id: None })
            .await
            .expect("submit(Cancel) ok");

        assert!(
            handle.active_cancel.lock().await.is_none(),
            "Cancel must not return until the cancelled turn releases its slot"
        );
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .expect("NewSession immediately after Cancel must not see an in-flight turn");
    });
}

/// A Block-behavior tool owns its mutation boundary until natural
/// completion. Cancel must wait instead of aborting the outer turn.
#[test]
fn submit_cancel_waits_for_blocking_owner_to_finish() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let turn = handle.reserve_turn(Some(7)).await.expect("reserve turn");
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let active = handle.active_cancel.clone();
        let task_turn = turn.clone();
        let parked = handle.runtime().spawn(async move {
            let _ = release_rx.await;
            let mut owner = active.lock().await;
            if owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &task_turn))
            {
                *owner = None;
            }
            drop(owner);
            task_turn.mark_completed();
        });
        turn.set_task_handle(parked);

        let cancelling_handle = handle.clone();
        let cancel_task =
            tokio::spawn(async move { cancelling_handle.cancel_active_turn(Some(7)).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !turn.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancel task must fire the matching token");
        assert!(
            !cancel_task.is_finished(),
            "Cancel must remain pending while the Block owner is active"
        );

        release_tx.send(()).expect("release Block owner");
        cancel_task
            .await
            .expect("cancel task joined")
            .expect("cancel completed");

        assert!(
            handle.active_cancel.lock().await.is_none(),
            "natural completion must release the single-turn slot"
        );
        assert!(turn.completed.load(std::sync::atomic::Ordering::Acquire));
    });
}

#[test]
fn submit_pause_waits_for_owner_and_publishes_paused_without_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                16,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("pause me"),
                None,
                Vec::new(),
            )
            .expect("begin durable turn");
        let turn = handle.reserve_turn(Some(16)).await.expect("reserve turn");
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let active = handle.active_cancel.clone();
        let task_turn = turn.clone();
        let parked = handle.runtime().spawn(async move {
            let _ = release_rx.await;
            let mut owner = active.lock().await;
            if owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &task_turn))
            {
                *owner = None;
            }
            drop(owner);
            task_turn.mark_completed();
        });
        turn.set_task_handle(parked);
        listener.received.lock().await.clear();

        let pausing_handle = handle.clone();
        let pause_task = tokio::spawn(async move {
            pausing_handle
                .submit(ClientCommand::PauseTurn {
                    turn_id: 16,
                    reason: "background_time_expired".to_string(),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !turn.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pause must fire the matching cancellation token");
        assert!(
            !pause_task.is_finished(),
            "Pause must remain pending while the owner is active"
        );

        release_tx.send(()).expect("release turn owner");
        pause_task
            .await
            .expect("pause task joined")
            .expect("pause completed");

        let checkpoint = handle
            .durable_turns
            .load(&session_id, 16)
            .expect("load paused checkpoint");
        assert_eq!(
            checkpoint.state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
        assert_eq!(
            checkpoint.reason.as_deref(),
            Some("background_time_expired")
        );
        assert!(handle.active_cancel.lock().await.is_none());

        let events = listener.received.lock().await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::TurnRecoveryState { snapshot }
                if snapshot.state == TurnRecoveryStateDto::PausedRecoverable
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ev::TurnEnded {
                outcome: client::protocol::events::TurnOutcomeDto::Cancelled,
                ..
            }
        )));
    });
}

#[test]
fn stale_specific_cancel_does_not_touch_current_turn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let turn = handle.reserve_turn(Some(8)).await.expect("reserve turn");
        handle
            .cancel_active_turn(Some(7))
            .await
            .expect("stale cancel is a no-op");

        assert!(!turn.cancel.is_cancelled());
        assert!(
            handle
                .active_cancel
                .lock()
                .await
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &turn)),
            "stale cancellation must retain the current owner"
        );

        *handle.active_cancel.lock().await = None;
        turn.mark_completed();
    });
}

#[test]
fn submit_cancel_terminalizes_inactive_paused_and_waiting_turns() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                91,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("paused"),
                None,
                Vec::new(),
            )
            .expect("begin paused turn");
        handle
            .durable_turns
            .transition(
                &session_id,
                91,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("backgrounded".to_string()),
            )
            .expect("pause turn");
        handle
            .durable_turns
            .begin(
                &session_id,
                92,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("waiting"),
                None,
                Vec::new(),
            )
            .expect("begin waiting turn");
        handle
            .durable_turns
            .transition(
                &session_id,
                92,
                TurnRecoveryStateDto::WaitingForUser,
                false,
                Some("waiting_for_user".to_string()),
            )
            .expect("waiting turn");
        listener.received.lock().await.clear();

        handle
            .submit(ClientCommand::Cancel { turn_id: Some(91) })
            .await
            .expect("cancel paused turn");
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(92) })
            .await
            .expect("cancel waiting turn");

        for turn_id in [91, 92] {
            let checkpoint = handle
                .durable_turns
                .load(&session_id, turn_id)
                .expect("load cancelled checkpoint");
            assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
            assert_eq!(checkpoint.reason.as_deref(), Some("explicit_cancel"));
        }
        let events = listener.received.lock().await;
        let cancelled: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Ev::TurnRecoveryState { snapshot }
                    if snapshot.state == TurnRecoveryStateDto::Cancelled =>
                {
                    Some(snapshot.turn_id)
                }
                _ => None,
            })
            .collect();
        assert_eq!(cancelled, vec![91, 92]);
    });
}

#[test]
fn submit_cancel_inactive_stale_terminal_and_other_session_are_noops() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                93,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("paused"),
                None,
                Vec::new(),
            )
            .expect("begin paused turn");
        handle
            .durable_turns
            .transition(
                &session_id,
                93,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("backgrounded".to_string()),
            )
            .expect("pause turn");
        handle
            .durable_turns
            .begin(
                &session_id,
                94,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("terminal"),
                None,
                Vec::new(),
            )
            .expect("begin terminal turn");
        handle
            .durable_turns
            .cancel(&session_id, 94)
            .expect("cancel terminal fixture");
        handle
            .durable_turns
            .begin(
                "other-session",
                95,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("foreign"),
                None,
                Vec::new(),
            )
            .expect("begin foreign turn");
        listener.received.lock().await.clear();

        for turn_id in [999, 94, 95] {
            handle
                .submit(ClientCommand::Cancel {
                    turn_id: Some(turn_id),
                })
                .await
                .expect("inactive cancel is safe");
        }

        assert_eq!(
            handle
                .durable_turns
                .load(&session_id, 93)
                .expect("load stale fixture")
                .state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
        assert_eq!(
            handle
                .durable_turns
                .load(&session_id, 94)
                .expect("load terminal fixture")
                .reason
                .as_deref(),
            Some("explicit_cancel")
        );
        assert_eq!(
            handle
                .durable_turns
                .load("other-session", 95)
                .expect("load foreign fixture")
                .state,
            Some(TurnRecoveryStateDto::Running)
        );
        assert!(listener.received.lock().await.is_empty());
    });
}

#[test]
fn submit_pause_cancels_only_owned_ask_user_question() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                96,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("pause"),
                None,
                Vec::new(),
            )
            .expect("begin turn");
        let turn = handle.reserve_turn(Some(96)).await.expect("reserve turn");
        let (owned_tx, owned_rx) = tokio::sync::oneshot::channel::<
            Vec<(
                lingxi_core::types::utf16_json::Utf16JsonProjection,
                lingxi_core::types::utf16_json::Utf16JsonProjection,
            )>,
        >();
        let active = handle.active_cancel.clone();
        let task_turn = turn.clone();
        let parked = handle.runtime().spawn(async move {
            tokio::select! {
                _ = owned_rx => {}
                _ = task_turn.cancel.cancelled() => {}
            }
            let mut owner = active.lock().await;
            if owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &task_turn))
            {
                *owner = None;
            }
            drop(owner);
            task_turn.mark_completed();
        });
        turn.set_task_handle(parked);
        handle
            .ask_user_question_tx
            .send(tool_ui::AskUserQuestionExchange {
                questions: Vec::new(),
                timeout_secs: None,
                resp_tx: owned_tx,
            })
            .await
            .expect("enqueue main-turn question");
        // A workflow question arrives while the main turn is already
        // active. It must survive the main turn's pause.
        let (unrelated_tx, mut unrelated_rx) = tokio::sync::oneshot::channel::<
            Vec<(
                lingxi_core::types::utf16_json::Utf16JsonProjection,
                lingxi_core::types::utf16_json::Utf16JsonProjection,
            )>,
        >();
        handle
            .ask_user_question_tx
            .send(tool_ui::AskUserQuestionExchange {
                questions: Vec::new(),
                timeout_secs: None,
                resp_tx: unrelated_tx,
            })
            .await
            .expect("enqueue concurrent workflow question");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.ask_user_question_broker.pending_count().await != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("main question parked and correlated");

        listener.received.lock().await.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.submit(ClientCommand::PauseTurn {
                turn_id: 96,
                reason: "background_time_expired".to_string(),
            }),
        )
        .await
        .expect("pause must not hang on a Block question")
        .expect("pause completed");

        assert_eq!(handle.ask_user_question_broker.pending_count().await, 1);
        assert!(matches!(
            unrelated_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            handle
                .durable_turns
                .load(&session_id, 96)
                .expect("load paused turn")
                .state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
    });
}

#[test]
fn submit_cancel_cancels_only_owned_ask_user_question() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                97,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("cancel"),
                None,
                Vec::new(),
            )
            .expect("begin turn");
        let turn = handle.reserve_turn(Some(97)).await.expect("reserve turn");
        let (owned_tx, owned_rx) = tokio::sync::oneshot::channel::<
            Vec<(
                lingxi_core::types::utf16_json::Utf16JsonProjection,
                lingxi_core::types::utf16_json::Utf16JsonProjection,
            )>,
        >();
        let active = handle.active_cancel.clone();
        let task_turn = turn.clone();
        let parked = handle.runtime().spawn(async move {
            tokio::select! {
                _ = owned_rx => {}
                _ = task_turn.cancel.cancelled() => {}
            }
            let mut owner = active.lock().await;
            if owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &task_turn))
            {
                *owner = None;
            }
            drop(owner);
            task_turn.mark_completed();
        });
        turn.set_task_handle(parked);
        handle
            .ask_user_question_tx
            .send(tool_ui::AskUserQuestionExchange {
                questions: Vec::new(),
                timeout_secs: None,
                resp_tx: owned_tx,
            })
            .await
            .expect("enqueue main-turn question");
        // This workflow question is concurrent with the active main turn
        // and must remain available after explicit Cancel.
        let (unrelated_tx, mut unrelated_rx) = tokio::sync::oneshot::channel::<
            Vec<(
                lingxi_core::types::utf16_json::Utf16JsonProjection,
                lingxi_core::types::utf16_json::Utf16JsonProjection,
            )>,
        >();
        handle
            .ask_user_question_tx
            .send(tool_ui::AskUserQuestionExchange {
                questions: Vec::new(),
                timeout_secs: None,
                resp_tx: unrelated_tx,
            })
            .await
            .expect("enqueue concurrent workflow question");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.ask_user_question_broker.pending_count().await != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("questions parked");

        listener.received.lock().await.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.submit(ClientCommand::Cancel { turn_id: Some(97) }),
        )
        .await
        .expect("cancel must not hang on a Block question")
        .expect("cancel completed");

        assert_eq!(handle.ask_user_question_broker.pending_count().await, 1);
        assert!(matches!(
            unrelated_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            handle
                .durable_turns
                .load(&session_id, 97)
                .expect("load cancelled turn")
                .state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
    });
}

/// The gate [`MobileEngineHandle::connection_sink`] exists to get past.
///
/// This is the NO-active-turn window, not the cancelled/quiescing one the
/// test below covers — two different mechanisms with the same observable
/// effect, so each needs an input only it can reach. Here every live-turn
/// payload is dropped, including the `SystemNotice` and
/// `CompactionCompleted` a command handler emits as its reply, while a
/// connection-scoped event still forwards. "Nothing arrived" would not have
/// distinguished the two.
#[tokio::test]
async fn lifecycle_listener_drops_turn_payload_when_no_turn_is_active() {
    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(None));
    let listener = super::TurnLifecycleListener::new(inner.clone(), active.clone());

    listener
        .on_event(Ev::SystemNotice {
            message: ClientTextDto::from("Message accepted for task a12345678"),
            is_error: false,
        })
        .await;
    listener
        .on_event(Ev::CompactionCompleted {
            messages_before: 2,
            messages_after: 1,
            bytes_saved: 10,
            summary: ClientTextDto::from("kept context"),
        })
        .await;
    listener
        .on_event(Ev::TaskLifecycle {
            event_json: "{\"type\":\"system\"}".to_string(),
        })
        .await;

    let seen = inner.received.lock().await;
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Ev::SystemNotice { .. })),
        "a command reply on SystemNotice is dropped with no turn to own it"
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Ev::CompactionCompleted { .. })),
        "so is the ForceCompact confirmation"
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, Ev::TaskLifecycle { .. })),
        "a connection-scoped event still forwards, so the drop above is the \
             turn gate and not a dead listener"
    );
}

#[tokio::test]
async fn lifecycle_listener_rewrites_cancelled_terminal_and_drops_late_events() {
    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(None));
    let turn = Arc::new(super::ActiveTurn::new(Some(12)));
    *active.lock().await = Some(turn.clone());
    let listener = super::TurnLifecycleListener::new(inner.clone(), active.clone());

    turn.cancel.cancel();
    listener
        .on_event(Ev::TurnEnded {
            outcome: client::protocol::events::TurnOutcomeDto::EndTurn,
            stop_reason: Some("end_turn".to_string()),
            cost: client::protocol::events::CostDto {
                total_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                api_calls: 0,
                session_duration_secs: 0,
                formatted: "$0.00".to_string(),
            },
        })
        .await;
    listener
        .on_event(Ev::ToolHeartbeat {
            id: "tool-1".to_string(),
            tool: "Bash".to_string(),
            elapsed_ms: 2_000,
        })
        .await;
    listener
        .on_event(Ev::ThinkingDelta {
            thinking: "late".to_string(),
            signature: None,
        })
        .await;
    listener
        .on_event(Ev::SystemNotice {
            message: ClientTextDto::from("late notice"),
            is_error: false,
        })
        .await;

    let events = inner.received.lock().await;
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events.first(),
        Some(Ev::TurnEnded {
            outcome: client::protocol::events::TurnOutcomeDto::Cancelled,
            stop_reason: Some(reason),
            ..
        }) if reason == "cancelled"
    ));
}

#[tokio::test]
async fn lifecycle_listener_drops_quiesced_events_without_cancel_rewrite() {
    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(None));
    let turn = Arc::new(super::ActiveTurn::new(Some(12)));
    turn.quiescing
        .store(true, std::sync::atomic::Ordering::Release);
    *active.lock().await = Some(turn);
    let listener = super::TurnLifecycleListener::new(inner.clone(), active);

    listener
        .on_event(Ev::TurnEnded {
            outcome: client::protocol::events::TurnOutcomeDto::EndTurn,
            stop_reason: Some("end_turn".to_string()),
            cost: client::protocol::events::CostDto {
                total_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                api_calls: 0,
                session_duration_secs: 0,
                formatted: "$0.00".to_string(),
            },
        })
        .await;
    listener
        .on_event(Ev::TextDelta {
            text: "late".into(),
        })
        .await;
    listener
        .on_event(Ev::Error {
            kind: client::protocol::events::ErrorKindDto::Internal,
            message: "late error".to_string().into(),
        })
        .await;

    assert!(
        inner.received.lock().await.is_empty(),
        "quiesced turns must not expose cancellation or late live events"
    );
}

#[tokio::test]
async fn lifecycle_listener_drops_unowned_live_payloads_but_forwards_questions() {
    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(None));
    let listener = super::TurnLifecycleListener::new(inner.clone(), active);

    listener
        .on_event(Ev::ToolUseStarted {
            id: "stale-tool".to_string(),
            tool: "Read".to_string(),
            input_json: "{}".to_string(),
            header: client::adapter::tool_display::lower_tool_header(
                "Read",
                &serde_json::json!({}),
            ),
        })
        .await;
    listener
        .on_event(Ev::ApiRetry {
            message: "late retry".to_string(),
            attempt: 1,
            max_retries: 3,
            delay_ms: 100,
        })
        .await;
    listener
        .on_event(Ev::SystemNotice {
            message: ClientTextDto::from("unowned notice"),
            is_error: false,
        })
        .await;
    listener
        .on_event(Ev::AskUserQuestion {
            request: client::protocol::ask_user_question::AskUserQuestionRequestDto {
                request_id: 7,
                questions: Vec::new(),
                timeout_secs: None,
            },
        })
        .await;

    let events = inner.received.lock().await;
    assert!(matches!(
        events.as_slice(),
        [Ev::AskUserQuestion { request }] if request.request_id == 7
    ));
}

#[tokio::test]
async fn late_connection_question_does_not_undo_a_paused_checkpoint() {
    for quiescing in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
            temp.path().join("turns"),
        ));
        store
            .begin("session-a", 12, "hello".into(), None, vec![])
            .unwrap();
        store
            .transition(
                "session-a",
                12,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("background_time_expired".into()),
            )
            .unwrap();
        let turn = Arc::new(super::ActiveTurn::new_owned(
            Some(12),
            "session-a".into(),
            1,
        ));
        if quiescing {
            turn.request_quiesce();
        }
        let active = Arc::new(tokio::sync::Mutex::new(Some(turn)));
        let inner = Arc::new(FakeListener::default());
        let listener =
            super::TurnLifecycleListener::new_durable(inner.clone(), active, store.clone());
        listener
            .on_event(Ev::AskUserQuestion {
                request: client::protocol::ask_user_question::AskUserQuestionRequestDto {
                    request_id: 7,
                    questions: vec![],
                    timeout_secs: None,
                },
            })
            .await;
        let checkpoint = store.load("session-a", 12).unwrap();
        assert_eq!(
            checkpoint.state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
        assert_eq!(
            checkpoint.reason.as_deref(),
            Some("background_time_expired")
        );
        assert!(checkpoint.safe_to_resume);
        let events = inner.received.lock().await;
        assert!(events.iter().any(
            |event| matches!(event, Ev::AskUserQuestion { request } if request.request_id == 7)
        ));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ev::TurnRecoveryState { .. })));
    }
}

#[tokio::test]
async fn lifecycle_listener_emits_raw_event_before_replay_ack() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    store
        .begin(
            "session-a",
            12,
            lingxi_core::types::utf16_json::Utf16JsonProjection::from("hello"),
            None,
            Vec::new(),
        )
        .expect("begin");

    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        super::ActiveTurn::new_owned(Some(12), "session-a".to_string(), 1),
    ))));
    let listener = super::TurnLifecycleListener::new_durable(inner.clone(), active, store);

    listener
        .on_event(Ev::TextDelta {
            text: "hello".into(),
        })
        .await;

    let events = inner.received.lock().await;
    assert!(matches!(
        events.as_slice(),
        [
            Ev::TextDelta { text },
            Ev::TurnEventReplay {
                session_id,
                turn_id: 12,
                sequence: 1,
                event_json,
            }
        ] if text.display() == "hello"
            && session_id == "session-a"
            && event_json.contains("\"text_delta\"")
    ));
}

fn media_handoff_context(session_id: lingxi_core::types::SessionId) -> tool_api::ToolUseContext {
    let mut context = tool_api::test_support::fresh_ctx();
    context.session = Some(Arc::new(tokio::sync::Mutex::new(
        lingxi_core::SessionState::empty(session_id, "test-model".into()),
    )));
    let assistant_message_id = lingxi_core::types::MessageId::new();
    let tool_use_id = lingxi_core::types::ToolUseId::new();
    context.assistant_message_id = Some(assistant_message_id.clone());
    context.tool_use_id = Some(tool_use_id.clone());
    context.assistant_message = Some(lingxi_core::types::ConversationMessage::Assistant {
        id: assistant_message_id,
        content: vec![lingxi_core::types::ContentBlock::ToolUse {
            id: tool_use_id,
            name: "media_session_start".into(),
            input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"intent":"conversation","mode":"audio"}),
            )
            .expect("test media tool input is valid JSON"),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
        per_turn_effort: None,
        resumable: None,
    });
    context
}

fn media_handoff_request() -> tool_api::MediaAgentHandoffRequest {
    tool_api::MediaAgentHandoffRequest {
        operation_id: media_api::MediaOperationId::new(1, 1),
        session: media_api::MediaSessionRef(media_api::MediaSessionId::from("media-session-1")),
        mode: media_api::MediaSessionMode::Audio,
    }
}

fn media_handoff_capability_context(
    session_id: lingxi_core::types::SessionId,
) -> orchestrator::MediaAgentCapabilityContext {
    orchestrator::MediaAgentCapabilityContext {
        session_id: session_id.as_uuid().to_string(),
        model: "test-model".into(),
        profile_name: None,
        account_scope: None,
    }
}

#[test]
fn media_handoff_queue_rejects_duplicate_and_wrong_session_requests() {
    use tool_api::MediaAgentHandoffHandle as _;

    let queue = super::MobileMediaAgentHandoffQueue::default();
    queue.set_attachment(Some(Arc::new(RecordingMediaAttachment::default())));
    let session_id = lingxi_core::types::SessionId::new();
    let session_key = session_id.as_uuid().to_string();
    queue.bind_session(Arc::new(StdMutex::new(session_key.clone())));
    let context = media_handoff_context(session_id);
    let capability_context = media_handoff_capability_context(session_id);

    queue
        .request(&context, &capability_context, media_handoff_request())
        .expect("first conversation request is queued");
    assert!(queue
        .request(&context, &capability_context, media_handoff_request())
        .is_err());

    let other_session = lingxi_core::types::SessionId::new();
    let other_context = media_handoff_context(other_session);
    assert!(queue
        .request(&other_context, &capability_context, media_handoff_request())
        .is_err());
    assert!(queue.has_pending_for_session(&session_key));
    assert!(!queue.has_pending_for_session(&other_session.as_uuid().to_string()));

    assert!(queue
        .take_for_turn(&other_session.as_uuid().to_string())
        .is_none());
    assert!(!queue.has_pending_for_session(&session_key));
    assert!(queue.take_for_turn(&session_key).is_none());

    queue
        .request(&context, &capability_context, media_handoff_request())
        .expect("request may be queued after stale ownership was cleared");
    let (_, old_generation) = queue.take_for_turn(&session_key).unwrap();
    assert!(queue.clear_for_session(&session_key).is_none());
    queue
        .request(&context, &capability_context, media_handoff_request())
        .expect("a released claim may queue a fresh handoff");
    let (_, new_generation) = queue.take_for_turn(&session_key).unwrap();
    assert_ne!(old_generation, new_generation);
    queue.finish_for_session(&session_key, old_generation);
    assert!(queue.has_pending_for_session(&session_key));
    queue.finish_for_session(&session_key, new_generation);
    assert!(!queue.has_pending_for_session(&session_key));

    queue
        .request(&context, &capability_context, media_handoff_request())
        .expect("session may queue another handoff after completion");
    let abandoned = queue
        .clear_for_session(&session_key)
        .expect("session retarget returns pending media for owner cleanup");
    assert_eq!(abandoned.request.session.0 .0, "media-session-1");
    assert!(!queue.has_pending_for_session(&session_key));
}

#[tokio::test]
async fn media_handoff_defers_terminal_until_the_owned_continuation_finishes() {
    use client::protocol::events::{CostDto, TurnOutcomeDto as Outcome};
    use tool_api::MediaAgentHandoffHandle as _;

    let session_id = lingxi_core::types::SessionId::new();
    let session_key = session_id.as_uuid().to_string();
    let queue = Arc::new(super::MobileMediaAgentHandoffQueue::default());
    queue.set_attachment(Some(Arc::new(RecordingMediaAttachment::default())));
    queue.bind_session(Arc::new(StdMutex::new(session_key.clone())));
    let context = media_handoff_context(session_id);
    let capability_context = media_handoff_capability_context(session_id);
    queue
        .request(&context, &capability_context, media_handoff_request())
        .expect("handoff request is queued");

    let root = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        root.path().join("turns"),
    ));
    store
        .begin(&session_key, 4, source_text("hello"), None, Vec::new())
        .expect("turn checkpoint begins");
    let turn = Arc::new(super::ActiveTurn::new_owned(
        Some(4),
        session_key.clone(),
        7,
    ));
    let active = Arc::new(tokio::sync::Mutex::new(Some(turn.clone())));
    let inner = Arc::new(FakeListener::default());
    let listener = super::TurnLifecycleListener::new_durable_with_media_handoffs(
        inner.clone(),
        active,
        store,
        queue.clone(),
    );
    let terminal = ClientEvent::TurnEnded {
        outcome: Outcome::EndTurn,
        stop_reason: Some("ordinary_turn_complete".into()),
        cost: CostDto {
            total_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            api_calls: 0,
            session_duration_secs: 0,
            formatted: "$0".into(),
        },
    };
    listener.on_event(terminal.clone()).await;
    assert!(inner.received.lock().await.is_empty());
    assert!(!turn
        .terminal_emitted
        .load(std::sync::atomic::Ordering::Acquire));
    assert!(turn.take_deferred_terminal().is_some());

    let (_, claim_generation) = queue
        .take_for_turn(&session_key)
        .expect("queued handoff enters its owned continuation");
    queue.finish_for_session(&session_key, claim_generation);
    listener.on_event(terminal).await;
    let events = inner.received.lock().await;
    assert!(matches!(
        events.first(),
        Some(ClientEvent::TurnEnded { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ClientEvent::TurnEnded { .. }))
            .count(),
        1
    );
    assert!(turn
        .terminal_emitted
        .load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn host_private_media_transport_events_are_not_turn_history() {
    use client::protocol::media::{
        MediaControlActionDto, MediaControlRequestDto, MediaSessionRefDto,
    };

    let events = [
        ClientEvent::MediaControlRequest {
            request: MediaControlRequestDto {
                request_id: "control-1".into(),
                connection_id: "connection-1".into(),
                media_session_ref: MediaSessionRefDto {
                    id: "media-session-1".into(),
                },
                generation: 1,
                action: MediaControlActionDto::FlushAudio {
                    target_sequence: 1,
                    target_sample_frames: 480,
                },
            },
        },
        ClientEvent::MediaServiceResponse {
            response: client::protocol::media::MediaServiceResponseDto {
                request_id: "service-1".into(),
                result: None,
                error: Some(client::protocol::media::MediaErrorDto {
                    kind: client::protocol::media::MediaErrorKindDto::Unavailable,
                    message: "host unavailable".into(),
                }),
            },
        },
    ];
    for event in &events {
        assert!(!super::TurnLifecycleListener::is_live_turn_payload(event));
        assert!(!super::TurnLifecycleListener::is_turn_journal_event(event));
    }
}

struct CancelledHandoffMediaService {
    audio: Arc<dyn media_api::MediaAudioChannel>,
    stop_calls: std::sync::atomic::AtomicUsize,
    session_audio_calls: std::sync::atomic::AtomicUsize,
}

impl CancelledHandoffMediaService {
    fn new(audio: Arc<dyn media_api::MediaAudioChannel>) -> Self {
        Self {
            audio,
            stop_calls: std::sync::atomic::AtomicUsize::new(0),
            session_audio_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[derive(Default)]
struct HandoffTestAudioChannel;

#[async_trait]
impl media_api::MediaAudioChannel for HandoffTestAudioChannel {
    async fn read(&self) -> Result<Option<media_api::MediaFrame>, media_api::MediaError> {
        Ok(None)
    }

    async fn write(
        &self,
        frame: media_api::MediaPcmFrame,
    ) -> Result<media_api::MediaRenderReceipt, media_api::MediaError> {
        Ok(media_api::MediaRenderReceipt {
            accepted_sample_frames: u32::try_from(
                frame.pcm16le.len() / (usize::from(frame.channels) * 2),
            )
            .unwrap_or(u32::MAX),
            played_sample_frames: 0,
            rendered_at_unix_ms: None,
        })
    }

    async fn flush(&self) -> Result<media_api::MediaRenderReceipt, media_api::MediaError> {
        Ok(media_api::MediaRenderReceipt {
            accepted_sample_frames: 0,
            played_sample_frames: 0,
            rendered_at_unix_ms: Some(1),
        })
    }

    async fn interrupt(&self) -> Result<media_api::MediaRenderReceipt, media_api::MediaError> {
        self.flush().await
    }
}

#[async_trait]
impl media_api::MediaService for CancelledHandoffMediaService {
    fn capabilities(&self) -> media_api::MediaCapabilities {
        media_api::MediaCapabilities {
            service_epoch: 1,
            support_revision: 1,
            supported_operations: vec![media_api::MediaOperationKind::Stop],
            supported_media_kinds: Vec::new(),
            readiness: Vec::new(),
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        _: media_api::MediaOperationContext,
        operation: media_api::MediaOperation,
    ) -> Result<media_api::MediaResult, media_api::MediaError> {
        match operation {
            media_api::MediaOperation::Stop { target } => {
                self.stop_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(media_api::MediaResult::Stopped { resource: target })
            }
            _ => Err(media_api::MediaError::new(
                media_api::MediaErrorKind::Unsupported,
                "test service supports cleanup only",
            )),
        }
    }

    async fn read_media(
        &self,
        _: media_api::MediaOperationContext,
        _: media_api::MediaRef,
    ) -> Result<media_api::MediaContent, media_api::MediaError> {
        Err(media_api::MediaError::new(
            media_api::MediaErrorKind::Unsupported,
            "not used",
        ))
    }

    async fn cancel(&self, _: media_api::MediaOperationId) -> Result<(), media_api::MediaError> {
        Ok(())
    }

    async fn session_audio(
        &self,
        context: media_api::MediaOperationContext,
        _: media_api::MediaSessionRef,
    ) -> Result<Arc<dyn media_api::MediaAudioChannel>, media_api::MediaError> {
        if !matches!(context.owner, media_api::MediaOwner::Session { .. }) {
            return Err(media_api::MediaError::new(
                media_api::MediaErrorKind::NotFound,
                "test owner mismatch",
            ));
        }
        self.session_audio_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.audio.clone())
    }
}

#[derive(Default)]
struct RecordingMediaAttachment {
    audio: StdMutex<Option<Arc<dyn media_api::MediaAudioChannel>>>,
}

#[async_trait]
impl super::MediaConversationAttachment for RecordingMediaAttachment {
    async fn open(
        &self,
        _: tool_api::MediaAgentHandoffRequest,
        _: orchestrator::RealtimeAgentContext,
        audio: Arc<dyn media_api::MediaAudioChannel>,
    ) -> Result<super::RealtimeAgentSessionIo, String> {
        *self.audio.lock().unwrap() = Some(audio);
        Err("test attachment captured the channel".into())
    }
}

struct ClosingMediaRealtimeCodec;

impl llm_runtime::services::sdk::realtime::RealtimeCodec for ClosingMediaRealtimeCodec {
    fn capabilities(&self) -> llm_runtime::services::sdk::realtime::RealtimeCapabilities {
        llm_runtime::services::sdk::realtime::RealtimeCapabilities {
            history_import: true,
            tools: true,
            input_transcription: true,
            output_transcription: true,
            interruption: true,
            ..Default::default()
        }
    }

    fn encode(
        &self,
        _: &llm_runtime::services::sdk::realtime::RealtimeInput,
    ) -> Result<
        Vec<llm_runtime::services::sdk::realtime::RealtimeFrame>,
        llm_runtime::services::sdk::realtime::RealtimeError,
    > {
        Ok(Vec::new())
    }

    fn decode(
        &self,
        _: llm_runtime::services::sdk::realtime::RealtimeFrame,
    ) -> Result<
        Vec<llm_runtime::services::sdk::realtime::RealtimeEvent>,
        llm_runtime::services::sdk::realtime::RealtimeError,
    > {
        Ok(Vec::new())
    }
}

struct ClosingMediaRealtimeSink;

#[async_trait]
impl llm_runtime::services::sdk::realtime::RealtimeSink for ClosingMediaRealtimeSink {
    async fn send(
        &mut self,
        _: llm_runtime::services::sdk::realtime::RealtimeFrame,
    ) -> Result<(), llm_runtime::services::sdk::realtime::RealtimeError> {
        Ok(())
    }

    async fn ping(
        &mut self,
        _: bytes::Bytes,
    ) -> Result<(), llm_runtime::services::sdk::realtime::RealtimeError> {
        Err(
            llm_runtime::services::sdk::realtime::RealtimeError::InvalidInput {
                message: "fixture transport does not support Ping".into(),
            },
        )
    }

    fn abort(&mut self) {}

    async fn close(
        &mut self,
        _: llm_runtime::services::sdk::realtime::RealtimeClose,
    ) -> Result<(), llm_runtime::services::sdk::realtime::RealtimeError> {
        Ok(())
    }
}

struct IdleMediaRealtimeTransport;

#[async_trait]
impl llm_runtime::services::sdk::realtime::RealtimeTransport for IdleMediaRealtimeTransport {
    async fn connect(
        &self,
        _: llm_runtime::services::sdk::realtime::RealtimeConnectRequest,
    ) -> Result<
        llm_runtime::services::sdk::realtime::RealtimeConnection,
        llm_runtime::services::sdk::realtime::RealtimeError,
    > {
        use futures_util::StreamExt as _;

        Ok(llm_runtime::services::sdk::realtime::RealtimeConnection {
            outbound: Box::new(ClosingMediaRealtimeSink),
            inbound: futures_util::stream::pending().boxed(),
        })
    }
}

struct SuccessfulHandoffMediaService {
    audio: Arc<dyn media_api::MediaAudioChannel>,
    start_calls: std::sync::atomic::AtomicUsize,
    stop_calls: std::sync::atomic::AtomicUsize,
    session_audio_calls: std::sync::atomic::AtomicUsize,
}

impl SuccessfulHandoffMediaService {
    fn new(audio: Arc<dyn media_api::MediaAudioChannel>) -> Self {
        Self {
            audio,
            start_calls: std::sync::atomic::AtomicUsize::new(0),
            stop_calls: std::sync::atomic::AtomicUsize::new(0),
            session_audio_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl media_api::MediaService for SuccessfulHandoffMediaService {
    fn capabilities(&self) -> media_api::MediaCapabilities {
        media_api::MediaCapabilities {
            service_epoch: 1,
            support_revision: 1,
            supported_operations: vec![
                media_api::MediaOperationKind::SessionStart,
                media_api::MediaOperationKind::Stop,
            ],
            supported_media_kinds: vec![media_api::MediaKind::Audio],
            readiness: Vec::new(),
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        _: media_api::MediaOperationContext,
        operation: media_api::MediaOperation,
    ) -> Result<media_api::MediaResult, media_api::MediaError> {
        match operation {
            media_api::MediaOperation::SessionStart { request }
                if request.intent == media_api::MediaSessionIntent::Conversation =>
            {
                self.start_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(media_api::MediaResult::SessionStarted {
                    session: media_api::MediaSessionRef(media_api::MediaSessionId::from(
                        "media-session-success",
                    )),
                    status: media_api::MediaSessionStatus {
                        state: media_api::MediaSessionState::Ready,
                        audio_enabled: true,
                        video_enabled: false,
                    },
                })
            }
            media_api::MediaOperation::Stop { target } => {
                self.stop_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(media_api::MediaResult::Stopped { resource: target })
            }
            _ => Err(media_api::MediaError::new(
                media_api::MediaErrorKind::Unsupported,
                "test service supports conversation start and cleanup only",
            )),
        }
    }

    async fn read_media(
        &self,
        _: media_api::MediaOperationContext,
        _: media_api::MediaRef,
    ) -> Result<media_api::MediaContent, media_api::MediaError> {
        Err(media_api::MediaError::new(
            media_api::MediaErrorKind::Unsupported,
            "not used",
        ))
    }

    async fn cancel(&self, _: media_api::MediaOperationId) -> Result<(), media_api::MediaError> {
        Ok(())
    }

    async fn session_audio(
        &self,
        context: media_api::MediaOperationContext,
        _: media_api::MediaSessionRef,
    ) -> Result<Arc<dyn media_api::MediaAudioChannel>, media_api::MediaError> {
        if !matches!(context.owner, media_api::MediaOwner::Session { .. }) {
            return Err(media_api::MediaError::new(
                media_api::MediaErrorKind::NotFound,
                "test owner mismatch",
            ));
        }
        self.session_audio_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.audio.clone())
    }
}

struct SuccessfulHandoffAttachment {
    expected_audio: Arc<dyn media_api::MediaAudioChannel>,
    same_channel: std::sync::atomic::AtomicBool,
    open_calls: std::sync::atomic::AtomicUsize,
    opened: tokio::sync::Notify,
    _io_guards: StdMutex<
        Option<(
            tokio::sync::mpsc::Sender<orchestrator::RealtimeAgentInput>,
            tokio::sync::mpsc::Receiver<llm_runtime::services::sdk::realtime::RealtimeEvent>,
        )>,
    >,
}

impl SuccessfulHandoffAttachment {
    fn new(expected_audio: Arc<dyn media_api::MediaAudioChannel>) -> Self {
        Self {
            expected_audio,
            same_channel: std::sync::atomic::AtomicBool::new(false),
            open_calls: std::sync::atomic::AtomicUsize::new(0),
            opened: tokio::sync::Notify::new(),
            _io_guards: StdMutex::new(None),
        }
    }
}

#[async_trait]
impl super::MediaConversationAttachment for SuccessfulHandoffAttachment {
    async fn open(
        &self,
        request: tool_api::MediaAgentHandoffRequest,
        _: orchestrator::RealtimeAgentContext,
        audio: Arc<dyn media_api::MediaAudioChannel>,
    ) -> Result<super::RealtimeAgentSessionIo, String> {
        if request.session.0 .0 != "media-session-success" {
            return Err("unexpected media session reference".into());
        }
        self.same_channel.store(
            Arc::ptr_eq(&self.expected_audio, &audio),
            std::sync::atomic::Ordering::SeqCst,
        );
        self.open_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.opened.notify_one();

        use llm_runtime::services::sdk::realtime::{
            RealtimeConnectRequest, RealtimeLimits, RealtimeSession,
        };
        let transport = IdleMediaRealtimeTransport;
        let (session, driver) = RealtimeSession::connect(
            &transport,
            RealtimeConnectRequest {
                endpoint: "wss://fixture".into(),
                headers: Vec::new(),
                max_frame_bytes: 1024,
            },
            Arc::new(ClosingMediaRealtimeCodec),
            RealtimeLimits::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
        tokio::spawn(driver.run());
        let (control, events) = session.into_parts();
        let (input_tx, inputs) = tokio::sync::mpsc::channel(8);
        let (output, output_rx) = tokio::sync::mpsc::channel(8);
        *self._io_guards.lock().unwrap() = Some((input_tx, output_rx));
        Ok(super::RealtimeAgentSessionIo {
            control,
            events,
            inputs,
            output,
            limits: orchestrator::RealtimeAgentLimits {
                idle_timeout: std::time::Duration::from_millis(300),
                max_duration: std::time::Duration::from_secs(5),
                ..Default::default()
            },
            image_input_supported: false,
        })
    }
}

#[test]
fn successful_media_handoff_reuses_channel_and_keeps_one_turn_owner() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config, HostFakePlatform};
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_start_tool_use, content_block_stop,
        input_json_delta, message_delta_stop, message_start, message_stop, text_delta,
        MockStreamingApiClient,
    };

    let root = tempfile::tempdir().expect("tempdir");
    let audio: Arc<dyn media_api::MediaAudioChannel> = Arc::new(HandoffTestAudioChannel);
    let service = Arc::new(SuccessfulHandoffMediaService::new(audio.clone()));
    let attachment = Arc::new(SuccessfulHandoffAttachment::new(audio));
    let tool_use_id = lingxi_core::types::ToolUseId::new();
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
        vec![
            message_start("media-start", "test-model"),
            content_block_start_tool_use(0, tool_use_id, "media_session_start"),
            input_json_delta(
                0,
                r#"{"intent":"conversation","mode":"audio","audio_source_ref":"mic-1"}"#,
            ),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ],
        vec![
            message_start("after-media-start", "test-model"),
            content_block_start_text(0),
            text_delta(0, "The media session is ready."),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
    ]));
    let mut config = test_config(root.path());
    std::fs::create_dir_all(&config.lingxi_home).expect("create fixture settings directory");
    std::fs::write(
        config.lingxi_home.join("settings.json"),
        r#"{"permissions":{"allow":["media_session_start"]}}"#,
    )
    .expect("allow the fixture media start through the production permission gate");
    config.media_service = Some(service.clone());
    config.media_conversation_attachment = Some(attachment.clone());
    let listener = Arc::new(FakeListener::default());
    let engine = new_engine_with_streaming(
        config,
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .expect("build mobile engine with media host");

    engine.runtime().block_on(async {
        engine
            .submit(ClientCommand::SendPrompt {
                text: ClientTextDto::from("Start an audio conversation"),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(71),
                visualization_context: None,
            })
            .await
            .expect("submit prompt");

        let attachment_opened = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            attachment.opened.notified(),
        )
        .await;
        if attachment_opened.is_err() {
            let captured_calls = streaming.captured_calls().await;
            let captured_requests = captured_calls
                .iter()
                .map(|call| {
                    (
                        call.model.clone(),
                        call.profile.clone(),
                        call.messages.len(),
                        call.tools
                            .iter()
                            .map(|tool| tool.name.clone())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            let events = listener.received.lock().await;
            panic!(
                "the committed media tool result must enter realtime under the same turn; start_calls={}, stop_calls={}, session_audio_calls={}, attachment_open_calls={}, captured_requests={captured_requests:?}, received_events={events:#?}",
                service.start_calls.load(std::sync::atomic::Ordering::SeqCst),
                service.stop_calls.load(std::sync::atomic::Ordering::SeqCst),
                service.session_audio_calls.load(std::sync::atomic::Ordering::SeqCst),
                attachment.open_calls.load(std::sync::atomic::Ordering::SeqCst),
            );
        }
        attachment_opened.expect("the committed media tool result must enter realtime under the same turn");
        assert!(
            !listener
                .received
                .lock()
                .await
                .iter()
                .any(|event| matches!(event, ClientEvent::TurnEnded { .. })),
            "the ordinary tool loop must not emit its terminal before realtime finishes"
        );
        assert!(engine.active_cancel.lock().await.is_some());

        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let ended = listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .any(|event| matches!(event, ClientEvent::TurnEnded { .. }));
                let idle = engine.active_cancel.lock().await.is_none();
                if ended && idle {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the same active turn continues and ends after native realtime");

        let events = listener.received.lock().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ClientEvent::TurnStarted { .. }))
                .count(),
            1,
            "the handoff must not reserve or start a second turn"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ClientEvent::TurnEnded { .. }))
                .count(),
            1,
            "the ordinary turn terminal is deferred until realtime finishes"
        );
        drop(events);
        assert!(
            attachment
                .same_channel
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert_eq!(
            attachment
                .open_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            service
                .start_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            service
                .session_audio_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            service.stop_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the media session is released once after realtime ends"
        );
    });
}

#[tokio::test]
async fn media_conversation_attachment_receives_the_existing_service_channel() {
    use media_api::MediaService as _;

    let audio: Arc<dyn media_api::MediaAudioChannel> = Arc::new(HandoffTestAudioChannel);
    let service = CancelledHandoffMediaService::new(audio.clone());
    let session_ref =
        media_api::MediaSessionRef(media_api::MediaSessionId::from("media-session-1"));
    let context = media_api::MediaOperationContext::new(
        media_api::MediaOperationId::new(1, 1),
        media_api::MediaOwner::Session {
            session_id: "session-1".into(),
        },
    );
    let existing_channel = service
        .session_audio(context, session_ref.clone())
        .await
        .expect("owner-bound session audio");
    let attachment = RecordingMediaAttachment::default();
    let request = tool_api::MediaAgentHandoffRequest {
        operation_id: media_api::MediaOperationId::new(2, 1),
        session: session_ref,
        mode: media_api::MediaSessionMode::Audio,
    };
    let prepared = orchestrator::RealtimeAgentContext {
        session_id: "session-1".into(),
        profile_name: Some("profile".into()),
        account_scope: Some("profile:profile".into()),
        instructions: String::new(),
        history: Vec::new(),
        tools: Vec::new(),
    };

    let _ = crate::media_handoff::open_attachment(
        &attachment,
        request,
        prepared,
        existing_channel.clone(),
    )
    .await;
    let attached = attachment.audio.lock().unwrap().clone().unwrap();
    assert!(Arc::ptr_eq(&existing_channel, &attached));
    assert_eq!(
        service
            .session_audio_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn cancelled_media_handoff_drains_under_the_existing_active_turn() {
    use tool_api::MediaAgentHandoffHandle as _;

    let root = tempfile::tempdir().expect("tempdir");
    let runtime = build_mobile(
        test_config(root.path()),
        Arc::new(HostFakePlatform::new(root.path().into())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .await
    .expect("build mobile runtime");
    let session = runtime.orchestrator.current_session_id().await;
    let session_key = session.as_uuid().to_string();
    let handoffs = Arc::new(super::MobileMediaAgentHandoffQueue::default());
    handoffs.set_attachment(Some(Arc::new(RecordingMediaAttachment::default())));
    handoffs.bind_session(Arc::new(StdMutex::new(session_key.clone())));
    let invocation = media_handoff_context(session);
    let capability_context = media_handoff_capability_context(session);
    handoffs
        .request(&invocation, &capability_context, media_handoff_request())
        .expect("handoff request is queued");

    let turn = Arc::new(super::ActiveTurn::new_owned(
        Some(9),
        session_key.clone(),
        11,
    ));
    turn.cancel.cancel();
    let service_impl = Arc::new(CancelledHandoffMediaService::new(Arc::new(
        HandoffTestAudioChannel,
    )));
    let service: Arc<dyn media_api::MediaService> = service_impl.clone();
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        super::continue_media_agent_handoff(
            runtime.orchestrator.clone(),
            turn,
            handoffs.clone(),
            Some(service),
            None,
            true,
        ),
    )
    .await
    .expect("handoff cleanup completes under the current turn")
    .expect("cancelled handoff is a normal terminal");
    assert_eq!(end, Some(orchestrator::RealtimeAgentEnd::Cancelled));
    assert!(!handoffs.has_pending_for_session(&session_key));
    assert_eq!(
        service_impl
            .stop_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cancelled handoff stops the media session exactly once"
    );
}

/// `cancel_active_turn` marks the durable checkpoint `Cancelled` BEFORE
/// the executor unwinds, so every non-terminal event the unwinding
/// executor still emits is refused by `append_event` with
/// `DurableTurnStoreError::Terminal`. That journal-only refusal must not
/// swallow CLIENT delivery: a Block tool that completes during
/// cancellation would otherwise stay "running" forever with no replay
/// path back.
#[tokio::test]
async fn lifecycle_listener_delivers_events_the_cancelled_journal_refuses() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    store
        .begin(
            "session-a",
            21,
            lingxi_core::types::utf16_json::Utf16JsonProjection::from("stop me"),
            None,
            Vec::new(),
        )
        .expect("begin");
    store.cancel("session-a", 21).expect("cancel");

    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        super::ActiveTurn::new_owned(Some(21), "session-a".to_string(), 1),
    ))));
    let listener = super::TurnLifecycleListener::new_durable(inner.clone(), active, store.clone());

    listener
        .on_event(Ev::ToolUseResult {
            id: "tool-1".to_string(),
            tool: "Bash".to_string(),
            result_json: "{}".to_string(),
            is_error: false,
            display: client::adapter::tool_display::lower_tool_result_display(
                "Bash",
                None,
                &serde_json::json!({}),
                false,
            ),
        })
        .await;

    let events = inner.received.lock().await;
    assert!(
        matches!(
            events.as_slice(),
            [Ev::ToolUseResult { id, .. }] if id == "tool-1"
        ),
        "a refused journal append must still deliver the raw event and \
             withhold only the sequenced replay envelope, got {events:?}"
    );
    assert_eq!(
        store
            .load("session-a", 21)
            .expect("load cancelled turn")
            .last_sequence,
        0,
        "the refused event must not advance the replay cursor"
    );
}

/// The `TurnEnded` arm consumes the `terminal_emitted` latch BEFORE the
/// journal write. If a refused write dropped the event, that burnt latch
/// would suppress every later terminal event and strand the client
/// streaming with no path back to Send.
#[tokio::test]
async fn lifecycle_listener_delivers_terminal_event_the_journal_refuses() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    store
        .begin(
            "session-a",
            22,
            lingxi_core::types::utf16_json::Utf16JsonProjection::from("done"),
            None,
            Vec::new(),
        )
        .expect("begin");
    store
        .transition(
            "session-a",
            22,
            TurnRecoveryStateDto::Completed,
            false,
            None,
        )
        .expect("complete");

    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        super::ActiveTurn::new_owned(Some(22), "session-a".to_string(), 1),
    ))));
    let listener = super::TurnLifecycleListener::new_durable(inner.clone(), active, store);

    listener
        .on_event(Ev::TurnEnded {
            outcome: client::protocol::events::TurnOutcomeDto::EndTurn,
            stop_reason: Some("end_turn".to_string()),
            cost: client::protocol::events::CostDto {
                total_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                api_calls: 0,
                session_duration_secs: 0,
                formatted: "$0.00".to_string(),
            },
        })
        .await;

    let events = inner.received.lock().await;
    assert!(
        matches!(events.first(), Some(Ev::TurnEnded { .. })),
        "the terminal event whose journal append was refused must still \
             reach the client that already consumed the latch, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ev::TurnEventReplay { .. })),
        "the refused event must not be acknowledged with a replay sequence, \
             got {events:?}"
    );
}

/// Connection-scoped listing/session/app/task events are not owned by the
/// live turn. Journaling them evicted real turn output from the bounded
/// retention window, replayed as envelopes the clients decode to null, and
/// made `can_resume_without_user` false for a turn that had produced
/// nothing of its own.
#[tokio::test]
async fn lifecycle_listener_keeps_connection_scoped_events_out_of_the_turn_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    store
        .begin(
            "session-a",
            23,
            lingxi_core::types::utf16_json::Utf16JsonProjection::from("list"),
            None,
            Vec::new(),
        )
        .expect("begin");

    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        super::ActiveTurn::new_owned(Some(23), "session-a".to_string(), 1),
    ))));
    let listener = super::TurnLifecycleListener::new_durable(inner.clone(), active, store.clone());

    listener
        .on_event(Ev::TurnStarted { turn_id: Some(23) })
        .await;
    listener
        .on_event(Ev::SessionList {
            sessions: Vec::new(),
        })
        .await;
    listener
        .on_event(Ev::ModelList {
            models: Vec::new(),
            current: "m".to_string(),
            current_model_source: None,
            details: Vec::new(),
        })
        .await;

    {
        let events = inner.received.lock().await;
        let replayed = events
            .iter()
            .filter_map(|event| match event {
                Ev::TurnEventReplay { event_json, .. } => Some(event_json.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            replayed.len(),
            1,
            "only the turn-owned turn_started may be sequenced, got {replayed:?}"
        );
        assert!(
            replayed[0].contains("\"turn_started\""),
            "the single retained envelope must be turn_started, got {replayed:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ev::SessionList { .. })),
            "the connection-scoped event must still reach the client live"
        );
    }

    let checkpoint = store.load("session-a", 23).expect("load");
    assert_eq!(
        checkpoint.last_sequence, 1,
        "session_list / model_list must not consume the retention budget"
    );

    store
        .transition(
            "session-a",
            23,
            TurnRecoveryStateDto::PausedRecoverable,
            true,
            Some("process_restarted".to_string()),
        )
        .expect("pause");
    let (disposition, _) = store.resume("session-a", 23).expect("resume");
    assert_eq!(
        disposition,
        crate::mobile::turn_durability::ResumeDisposition::Ready,
        "a turn that only emitted turn_started must stay auto-resumable"
    );
}

/// `AskUserQuestion` is connection-scoped, so its recovery transition needs
/// the reverse edge the broker always emits. Without it the turn stayed
/// labelled `WaitingForUser` for the rest of its life.
#[tokio::test]
async fn lifecycle_listener_restores_running_when_a_question_resolves() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    store
        .begin(
            "session-a",
            24,
            lingxi_core::types::utf16_json::Utf16JsonProjection::from("ask"),
            None,
            Vec::new(),
        )
        .expect("begin");

    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        super::ActiveTurn::new_owned(Some(24), "session-a".to_string(), 1),
    ))));
    let listener = super::TurnLifecycleListener::new_durable(inner.clone(), active, store.clone());

    listener
        .on_event(Ev::AskUserQuestion {
            request: client::protocol::ask_user_question::AskUserQuestionRequestDto {
                request_id: 5,
                questions: Vec::new(),
                timeout_secs: None,
            },
        })
        .await;
    assert_eq!(
        store.load("session-a", 24).expect("load parked").state,
        Some(TurnRecoveryStateDto::WaitingForUser)
    );

    listener
        .on_event(Ev::AskUserQuestionResolved { request_id: 5 })
        .await;

    let checkpoint = store.load("session-a", 24).expect("load resolved");
    assert_eq!(
        checkpoint.state,
        Some(TurnRecoveryStateDto::Running),
        "AskUserQuestionResolved must restore Running"
    );
    assert!(
        !checkpoint.safe_to_resume,
        "a resolved question is still a side effect: re-running the prompt \
             stays user-gated"
    );
    let snapshots = inner
        .received
        .lock()
        .await
        .iter()
        .filter_map(|event| match event {
            Ev::TurnRecoveryState { snapshot } => Some(snapshot.state.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        snapshots,
        vec![
            TurnRecoveryStateDto::WaitingForUser,
            TurnRecoveryStateDto::Running
        ],
        "the client must observe both edges"
    );
}

/// `PauseTurn` for a turn that does not own the live executor slot used to
/// return `Ok(())` while emitting NOTHING. The iOS client parks an untimed
/// continuation in `pauseAcknowledgements` that only the acknowledging
/// event resolves, so the silent answer deadlocked it.
#[test]
fn submit_pause_for_a_non_active_turn_still_acknowledges() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle
            .durable_turns
            .begin(
                &session_id,
                41,
                lingxi_core::types::utf16_json::Utf16JsonProjection::from("background"),
                None,
                Vec::new(),
            )
            .expect("begin background turn");
        // A DIFFERENT turn owns the live executor slot.
        *handle.active_cancel.lock().await = Some(Arc::new(super::ActiveTurn::new_owned(
            Some(42),
            session_id.clone(),
            7,
        )));
        listener.received.lock().await.clear();

        handle
            .submit(ClientCommand::PauseTurn {
                turn_id: 41,
                reason: "background_time_expired".to_string(),
            })
            .await
            .expect("pause accepted");

        let events = listener.received.lock().await;
        let snapshots = events
            .iter()
            .filter_map(|event| match event {
                ClientEvent::TurnRecoveryState { snapshot } => Some(snapshot.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            snapshots.len(),
            1,
            "a pause for a non-active turn must emit exactly one \
                 acknowledging TurnRecoveryState, got {events:?}"
        );
        assert_eq!(snapshots[0].turn_id, 41);
        assert_eq!(snapshots[0].state, TurnRecoveryStateDto::PausedRecoverable);
        assert_eq!(
            handle
                .active_cancel
                .lock()
                .await
                .as_ref()
                .and_then(|turn| turn.turn_id),
            Some(42),
            "the unrelated live turn must be left alone"
        );
    });
}

/// A connection owns at most one live turn. A second `SendPrompt` joins
/// the bounded mid-turn queue instead of replacing the first turn's owner.
#[test]
fn submit_send_prompt_queues_overlapping_turn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        *handle.active_cancel.lock().await = Some(Arc::new(super::ActiveTurn::new(None)));

        let result = handle
            .submit(ClientCommand::SendPrompt {
                text: "pending guidance".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(99),
                visualization_context: None,
            })
            .await;

        assert!(
            result.is_ok(),
            "overlapping SendPrompt must queue: {result:?}"
        );
        let session_id = handle.active_session_id();
        let pending = handle
            .durable_turns
            .scan_pending(&session_id)
            .expect("busy prompt is durably admitted before acceptance");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].turn_id, 99);
        assert_eq!(pending[0].original_client_turn_id, Some(99));
        assert_eq!(pending[0].prompt.string_at(""), Some("pending guidance"));
        assert_eq!(pending[0].state, None);
        assert_eq!(
            handle
                .message_queue
                .snapshot()
                .await
                .iter()
                .filter(|command| command.uuid == format!("mobile-prompt:{session_id}:99"))
                .count(),
            1,
            "the volatile queue contains one dispatch cache entry"
        );
    });
}

/// A `AskUserQuestion` response only touches the connection-scoped broker.
/// It must not wait behind a transition held by the workflow that asked it,
/// otherwise iOS leaves the native question sheet permanently submitting.
#[test]
fn submit_question_answer_bypasses_a_held_transition_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        handle
            .ask_user_question_tx
            .send(tool_ui::AskUserQuestionExchange {
                questions: Vec::new(),
                timeout_secs: None,
                resp_tx: response_tx,
            })
            .await
            .expect("enqueue question");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.ask_user_question_broker.pending_count().await != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("question must be parked");

        let question_units = vec![
            u16::from(b'W'),
            u16::from(b'h'),
            u16::from(b'e'),
            u16::from(b'n'),
            u16::from(b'?'),
            0xD800,
        ];
        let answer_units = vec![u16::from(b'N'), u16::from(b'o'), u16::from(b'w'), 0xDFFF];
        let transition = handle.loop_transition.lock().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.submit(ClientCommand::AnswerAskUserQuestion {
                request_id: 1,
                answers: client::protocol::ask_user_question::AskUserQuestionAnswersDto {
                    pairs: vec![
                        client::protocol::ask_user_question::AskUserQuestionAnswerDto {
                            question: client::protocol::text::ClientTextDto::from_code_units(
                                question_units.clone(),
                            ),
                            answer: client::protocol::text::ClientTextDto::from_code_units(
                                answer_units.clone(),
                            ),
                        },
                    ],
                },
            }),
        )
        .await
        .expect("answer must not wait for the transition lock")
        .expect("answer accepted");
        drop(transition);

        let broker_answers = response_rx.await.expect("broker response");
        assert_eq!(broker_answers.len(), 1);
        assert_eq!(broker_answers[0].0.string_units(""), Some(question_units));
        assert_eq!(broker_answers[0].1.string_units(""), Some(answer_units));
        assert!(listener.received.lock().await.iter().any(|event| matches!(
            event,
            ClientEvent::AskUserQuestionResolved { request_id: 1 }
        )));
    });
}

/// Mod UI control responses are session-bound but not turn-owned. They must
/// remain correlated and answerable while an unrelated turn holds the normal
/// command transition lock.
#[test]
fn submit_mod_ui_control_bypasses_turn_transition_and_returns_correlated_result() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let _transition = handle.loop_transition.lock().await;
        *handle.active_cancel.lock().await = Some(Arc::new(super::ActiveTurn::new(None)));

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.submit(ClientCommand::UiClientModule {
                request_id: "module-while-turn-active".into(),
                plugin: "uninstalled-plugin".into(),
            }),
        )
        .await
        .expect("UI control must not wait on an unrelated turn transition")
        .expect("UI control command is acknowledged through its response event");

        let events = listener.received.lock().await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                ClientEvent::UiControlResult {
                    request_id,
                    response_json,
                    error,
                    ..
                } if request_id == "module-while-turn-active"
                    && (response_json.is_some() || error.is_some())
            )),
            "the current handle must answer the correlated UI request: {events:?}"
        );
    });
}

/// A provider/model failure is terminal for the connection slot just like a
/// successful or cancelled turn. The orchestrator surfaces authentication
/// failure as a `model_error` turn, then session control becomes available.
#[test]
fn fixed_loop_scheduler_delivers_one_meta_mobile_turn_without_dream_agent() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::test_support_stream::*;
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(FakeListener::default());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("fixed-loop-mobile", "test"),
        content_block_start_text(0),
        text_delta(0, "fixed tick complete"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        assert!(
            handle.session_cron.is_some(),
            "mobile construction binds a real session cron scheduler"
        );
        let registry: Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle> =
            handle.inner.task_registry.clone();
        assert!(!handle.message_queue.has_main_thread_commands().await);
        assert!(
            !handle
                .inner
                .task_registry
                .has_pending_task_notifications_for(None)
                .await
        );
        // Keep the real consumer from taking the first fire until its queue
        // metadata is inspected; the actual delivery producer stays live.
        let watcher_gate = handle.active_cancel.lock().await;
        cron::register_live_job(
            &registry,
            cron::SessionCronTask {
                id: "00000000".into(),
                cron: "* * * * *".into(),
                prompt: "mobile fixed loop owned task".into(),
                created_at: std::time::SystemTime::now() - std::time::Duration::from_secs(120),
                last_fired_at: None,
                recurring: false,
                owner: None,
            },
            false,
        )
        .await
        .unwrap();
        let queued = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let commands = handle
                    .message_queue
                    .get_by_max_priority(msgqueue::QueuePriority::Later, |command| {
                        command.scheduled_task_id.as_deref() == Some("00000000")
                    })
                    .await;
                if !commands.is_empty() {
                    break commands;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the real cron tick must enqueue the fixed fire");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].source, msgqueue::QueueSource::Cron);
        assert_eq!(queued[0].priority, msgqueue::QueuePriority::Later);
        assert!(queued[0].is_meta);
        assert!(queued[0].skip_slash_commands);
        assert_eq!(queued[0].text(), Some("mobile fixed loop owned task"));
        assert!(
            cron::session_jobs(&registry).await.unwrap().is_empty(),
            "one-shot is claimed once"
        );
        drop(watcher_gate);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !streaming.captured_calls().await.is_empty()
                    && handle.active_cancel.lock().await.is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the production mobile consumer must complete the main turn");
        let calls = streaming.captured_calls().await;
        assert_eq!(calls.len(), 1);
        assert!(serde_json::to_string(&calls[0].messages)
            .unwrap()
            .contains("mobile fixed loop owned task"));
        assert!(!handle
            .inner
            .task_registry
            .list()
            .await
            .iter()
            .any(|task| matches!(task, tasks::state::TaskState::Dream(_))));
        assert!(!listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, Ev::LoopWakeup { .. })));
        assert!(!handle.message_queue.has_active_turn().await);
    });
}

#[test]
fn dynamic_loop_scheduler_drives_a_mobile_turn_with_session_cwd() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::test_support_stream::*;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    std::fs::write(tmp.path().join(".claude/loop.md"), "mobile loop owned task").unwrap();
    let listener = Arc::new(FakeListener::default());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("loop-mobile", "test"),
        content_block_start_text(0),
        text_delta(0, "mobile tick complete"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let scheduler = handle
            .inner
            .wakeup_scheduler
            .get()
            .expect("registered tool cell is bound");
        scheduler
            .schedule(
                std::time::Duration::ZERO,
                "<<loop.md-dynamic>>".into(),
                "test fire".into(),
            )
            .await;
        for _ in 0..3000 {
            if !streaming.captured_calls().await.is_empty()
                && handle.active_cancel.lock().await.is_none()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let calls = streaming.captured_calls().await;
        assert_eq!(calls.len(), 1, "scheduled tick must reach the model once");
        let messages = serde_json::to_string(&calls[0].messages).unwrap();
        assert!(messages.contains("mobile loop owned task"), "{messages}");
        assert!(listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, Ev::LoopWakeup { .. })));
        assert!(!handle.message_queue.has_active_turn().await);
        assert!(scheduler
            .loop_runtime()
            .unwrap()
            .in_flight_prompt()
            .is_none());
    });
}

#[test]
fn dynamic_loop_idle_cancel_removes_timer_and_raw_queued_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        let scheduler = handle.inner.wakeup_scheduler.get().unwrap();
        scheduler
            .schedule(
                std::time::Duration::from_secs(600),
                "raw mobile loop".into(),
                "pending".into(),
            )
            .await;
        handle
            .message_queue
            .enqueue(msgqueue::QueuedCommand {
                scheduled_task_id: None,
                scheduled_fire_id: None,
                uuid: "loop-wakeup-test".into(),
                content: msgqueue::QueuedCommandContent::UserInput {
                    text: lingxi_core::types::utf16_json::Utf16JsonProjection::from(
                        "queued raw loop",
                    ),
                },
                priority: msgqueue::QueuePriority::Later,
                queued_at: std::time::SystemTime::now(),
                source: msgqueue::QueueSource::Cron,
                agent_id: None,
                skip_slash_commands: true,
                is_meta: true,
            })
            .await;
        handle
            .submit(ClientCommand::Cancel { turn_id: None })
            .await
            .unwrap();
        assert!(scheduler.cancel_pending().await.is_empty());
        assert!(!handle.message_queue.has_main_thread_commands().await);
        assert!(scheduler.loop_runtime().unwrap().loop_ended());
    });
}

#[test]
fn dynamic_loop_session_switch_cancels_previous_session_timer() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        let scheduler = handle.inner.wakeup_scheduler.get().unwrap();
        scheduler
            .schedule(
                std::time::Duration::from_secs(600),
                "old session task".into(),
                "pending".into(),
            )
            .await;
        let old_id = handle.active_session_id();
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        assert_ne!(handle.active_session_id(), old_id);
        assert!(scheduler.cancel_pending().await.is_empty());
        assert!(scheduler
            .loop_runtime()
            .unwrap()
            .in_flight_prompt()
            .is_none());
    });
}

#[test]
fn mobile_session_switch_ends_only_the_previous_audio_owner() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let audio = Arc::new(AudioOwnerTeardownProbe::default());
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(AudioHostFakePlatform {
        base: HostFakePlatform::new(tmp.path().to_path_buf()),
        audio: audio.clone(),
    });
    let (handle, _) = build_submit_handle_with_platform(tmp.path(), platform);
    let previous_session = handle.active_session_id();
    let previous_owner = AudioOwner::Session {
        session_id: previous_session,
    };
    let unrelated_owner = AudioOwner::Session {
        session_id: "unrelated-session".into(),
    };
    handle.runtime().block_on(async {
        {
            let mut handles = handle
                .inner
                .mcp_tool_context
                .audio_recording_handles
                .lock()
                .await;
            handles.insert(
                previous_owner.clone(),
                media_api::RecordingRef(media_api::RecordingId::from("old-recording")),
            );
            handles.insert(
                unrelated_owner.clone(),
                media_api::RecordingRef(media_api::RecordingId::from("other-recording")),
            );
        }

        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .expect("new session should be created");

        let handles = handle
            .inner
            .mcp_tool_context
            .audio_recording_handles
            .lock()
            .await;
        assert!(!handles.contains_key(&previous_owner));
        assert!(handles.contains_key(&unrelated_owner));
    });

    let operations = audio.operations.lock().unwrap().clone();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].0, previous_owner);
    assert!(matches!(&operations[0].1, AudioOperation::EndOwner));
}

#[test]
fn mobile_turn_end_preserves_recording_until_engine_disposal() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::test_support_stream::*;

    let tmp = tempfile::tempdir().expect("tempdir");
    let listener = Arc::new(FakeListener::default());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("audio-owner-lifecycle", "test"),
        content_block_start_text(0),
        text_delta(0, "turn completed"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let audio = Arc::new(AudioOwnerTeardownProbe::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(AudioHostFakePlatform {
            base: HostFakePlatform::new(tmp.path().to_path_buf()),
            audio: audio.clone(),
        }),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming),
    )
    .expect("mobile engine builds");
    let session_id = handle.active_session_id();
    let owner = AudioOwner::Session {
        session_id: session_id.clone(),
    };
    handle.runtime().block_on(async {
        handle
            .inner
            .mcp_tool_context
            .audio_recording_handles
            .lock()
            .await
            .insert(
                owner.clone(),
                media_api::RecordingRef(media_api::RecordingId::from("live-recording")),
            );

        handle
            .submit(ClientCommand::SendPrompt {
                text: "finish this turn".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(71),
                visualization_context: None,
            })
            .await
            .expect("prompt is accepted");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let ended = listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .any(|event| matches!(event, Ev::TurnEnded { .. }));
                if ended && handle.active_cancel.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the normal turn should finish");

        assert!(handle
            .inner
            .mcp_tool_context
            .audio_recording_handles
            .lock()
            .await
            .contains_key(&owner));
    });
    assert!(audio.operations.lock().unwrap().is_empty());

    drop(handle);
    let operations = audio.operations.lock().unwrap();
    let (operations, wait) = audio
        .operations_changed
        .wait_timeout_while(
            operations,
            std::time::Duration::from_secs(2),
            |operations| operations.is_empty(),
        )
        .unwrap();
    assert!(!wait.timed_out(), "engine disposal should end its owner");
    let operations = operations.clone();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].0, owner);
    assert!(matches!(&operations[0].1, AudioOperation::EndOwner));
}

#[test]
fn mobile_drop_does_not_wait_for_ui_bound_audio_callback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(AudioDropGateService {
        state: Arc::new((
            StdMutex::new(AudioDropGateState::default()),
            Default::default(),
        )),
    });
    let state = service.state.clone();
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(AudioHostFakePlatform {
        base: HostFakePlatform::new(tmp.path().to_path_buf()),
        audio: service,
    });
    let (handle, _) = build_submit_handle_with_platform(tmp.path(), platform);
    let (drop_tx, drop_rx) = std::sync::mpsc::channel();
    let drop_thread = std::thread::spawn(move || {
        drop(handle);
        let _ = drop_tx.send(());
    });

    let (state_lock, changed) = state.as_ref();
    let state_guard = state_lock.lock().unwrap();
    let (mut state_guard, wait) = changed
        .wait_timeout_while(state_guard, std::time::Duration::from_secs(2), |state| {
            !state.callback_started
        })
        .unwrap();
    assert!(
        state_guard.callback_started,
        "the native cleanup callback should be dispatched"
    );
    let drop_returned_before_callback = drop_rx
        .recv_timeout(std::time::Duration::from_millis(250))
        .is_ok();
    state_guard.drop_returned = true;
    changed.notify_all();
    drop(state_guard);

    if !drop_returned_before_callback {
        drop_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("opening the UI callback gate must release a synchronous drop");
    }
    drop_thread.join().expect("drop thread should complete");

    let state_guard = state_lock.lock().unwrap();
    let (state_guard, completion_wait) = changed
        .wait_timeout_while(state_guard, std::time::Duration::from_secs(2), |state| {
            !state.callback_completed
        })
        .unwrap();
    assert!(
        state_guard.callback_completed && !completion_wait.timed_out(),
        "the callback should complete after the caller releases its executor"
    );
    assert!(
        drop_returned_before_callback,
        "engine Drop must return before native callbacks that wait on the UI executor"
    );
    assert!(!wait.timed_out());
}

#[test]
fn submit_model_error_releases_slot() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let listener_dyn: Arc<dyn ClientEventListener> = listener.clone();
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let streaming: Arc<dyn orchestrator::StreamingApiClient> =
        Arc::new(MockStreamingApiClient::with_open_error(
            llm_runtime::LlmError::Authentication {
                message: String::new(),
            },
            Vec::new(),
        ));
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        platform,
        listener_dyn,
        perm_sink,
        Some(streaming),
    )
    .expect("build_mobile_engine failed");

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SendPrompt {
                text: "fail deterministically".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(100),
                visualization_context: None,
            })
            .await
            .expect("submit(SendPrompt) ok");

        for _ in 0..2000 {
            let failed = listener.received.lock().await.iter().any(|event| {
                matches!(
                    event,
                    Ev::TurnEnded { stop_reason, .. }
                        if stop_reason.as_deref() == Some("model_error")
                )
            });
            if failed && handle.active_cancel.lock().await.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let events = listener.received.lock().await.clone();
        assert!(
            events.iter().any(|event| matches!(
                event,
                Ev::TurnEnded { stop_reason, .. }
                    if stop_reason.as_deref() == Some("model_error")
            )),
            "a provider failure must terminate as model_error: {events:?}"
        );
        assert!(
            handle.active_cancel.lock().await.is_none(),
            "a failed turn must release its connection slot"
        );
        handle
            .submit(ClientCommand::ClearSession)
            .await
            .expect("session control must work after a failed turn");
    });
}

/// F3-05: `submit(ApprovePermission)` resolves a parked `check()` oneshot on
/// the connection-scoped [`AdapterPermissionGate`] (F1-14) — the inbound
/// command side of the inverted permission handshake. A parked `check()`
/// returns `Allow` once the approval arrives via `submit`.
#[test]
fn submit_approve_resolves_oneshot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        // Park a real `check()` on the gate from a spawned task (the engine
        // turn side); it blocks on the oneshot until `submit` resolves it.
        let gate = handle.permission_gate();
        let g = gate.clone();
        let parked = handle.runtime().spawn(async move {
            use permission::gate::PermissionGate;
            g.check("Bash", &source_value(&serde_json::json!({"command": "ls"})))
                .await
        });

        // Spin until the gate has parked exactly one request (id starts at
        // 1). A short async sleep (not a bare `yield_now`) lets the spawned
        // `check()` make progress even if the worker pool is momentarily busy.
        for _ in 0..2000 {
            if gate.pending_count().await == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(gate.pending_count().await, 1, "check() must park a request");

        // Approve via the FFI command path — resolves the parked oneshot.
        handle
            .submit(ClientCommand::ApprovePermission {
                request_id: 1,
                response: PermissionResponseDto::AllowOnce,
            })
            .await
            .expect("submit(ApprovePermission) ok");

        let decision = parked.await.expect("parked check joined");
        assert_eq!(decision, permission::gate::PermissionDecision::Allow);
    });
}

// ── F3-07: async-over-FFI runtime registration ──────────────────────────

/// F3-07: the async FFI exports resolve on the HANDLE-OWNED tokio runtime.
///
/// `UniFFI`'s `#[uniffi::export(async_runtime = "tokio")]` (plus the workspace
/// `uniffi` dep's `tokio` feature, pinned in F3-00) registers a tokio runtime
/// as the foreign async executor; the mobile host registers the
/// handle-owned `rt-multi-thread` runtime (§0.5 — one connection ⇒ one engine
/// host owning one runtime). This proves the registration actually takes: the
/// async `observed_runtime_id` export — driven through the SAME executor path
/// `submit` uses — resolves on the runtime whose id matches the handle's owned
/// runtime, NOT a transient ambient one.
#[test]
fn async_submit_resolves_on_handle_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());

    // The handle-owned runtime's identity token (the runtime F3-07 registers
    // as the foreign async executor).
    let owned_id = handle.runtime_id();

    // Drive the async export on the handle-owned runtime — exactly what the
    // UniFFI tokio foreign executor does for a foreign caller. The future
    // awaits (a real yield point) and then reads the runtime it is bound to.
    let observed_id = handle
        .runtime()
        .block_on(async { handle.observed_runtime_id().await });

    assert_eq!(
        observed_id, owned_id,
        "the async FFI export must resolve on the handle-owned tokio runtime \
             (foreign async executor = the handle's runtime), got {observed_id} vs owned {owned_id}"
    );

    // And the inbound `submit` async export resolves on that same runtime: a
    // `Cancel` (no in-flight turn) drives the full `submit` future through the
    // executor and returns `Ok` — proving the async entry point itself awaits
    // on the registered runtime, not just the inspection helper.
    let cancel_result = handle
        .runtime()
        .block_on(async { handle.submit(ClientCommand::Cancel { turn_id: None }).await });
    assert!(
        cancel_result.is_ok(),
        "submit must resolve on the handle-owned runtime: {cancel_result:?}"
    );
}

// ── SESSIONS/HISTORY: ListSessions / NewSession / ResumeSession ──────────

/// Seed one valid session JSONL under `<lingxi_home>/projects/<sanitize(cwd)>/`
/// so `submit(ListSessions)` has a real on-disk catalog to enumerate. Mirrors
/// the `session` crate's own `list_recent_test` fixture (the enumerator reads
/// the dir via `tokio::fs` and each file via the injected `fs`). Returns the
/// seeded session UUID string.
fn seed_session_file(root: &std::path::Path) -> String {
    let cfg = test_config(root);
    let cwd = cfg.cwd.to_string_lossy().into_owned();
    let project_dir = cfg
        .lingxi_home
        .join("projects")
        .join(session::jsonl::project_dir_name(&cwd));
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    // A fixed, valid UUID literal (the loader parses the filename stem with
    // `Uuid::parse_str`; harness-runtime::mobile does not depend on the `uuid` crate, so
    // we use a literal instead of minting one). Deterministic by design.
    let uuid = "11111111-2222-3333-4444-555555555555".to_string();
    let path = project_dir.join(format!("{uuid}.jsonl"));
    let line = serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "parentUuid": serde_json::Value::Null,
        "sessionId": uuid,
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": "hello from a prior session"}
    });
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .expect("write session file");
    uuid
}

/// Drain every event the listener received during a blocked closure.
async fn drained(listener: &FakeListener) -> Vec<Ev> {
    listener.received.lock().await.clone()
}

/// SESSIONS/HISTORY: `submit(ListSessions)` enumerates the on-disk catalog and
/// emits a `SessionList` carrying the seeded row (proving the engine actually
/// reads the store — not a no-op catch-all). The row's `uuid` matches the
/// seeded file's stem, lowered via the shared `lower_session_metadata`.
#[test]
fn submit_list_sessions_emits_seeded_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seeded_uuid = seed_session_file(tmp.path());
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ListSessions { limit: None })
            .await
            .expect("submit(ListSessions) ok");

        let events = drained(&listener).await;
        let row = events.iter().find_map(|e| match e {
            Ev::SessionList { sessions } => Some(sessions.clone()),
            _ => None,
        });
        let sessions = row.expect("a SessionList event must be emitted");
        assert_eq!(sessions.len(), 1, "exactly one seeded session expected");
        assert_eq!(
            sessions[0].uuid, seeded_uuid,
            "the listed row must be the seeded session"
        );
        assert_eq!(sessions[0].mode, SessionModeDto::Code);
    });
}

/// SESSIONS/HISTORY: `submit(ListSessions)` on a connection with NO on-disk
/// catalog (empty / missing project dir) still replies with a `SessionList`
/// carrying an EMPTY vec — the loader's `EmptyDirectory` is "no sessions yet",
/// not an error, and the client must always get a reply.
#[test]
fn submit_list_sessions_empty_when_no_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ListSessions { limit: Some(5) })
            .await
            .expect("submit(ListSessions) ok");

        let events = drained(&listener).await;
        let sessions = events
            .iter()
            .find_map(|e| match e {
                Ev::SessionList { sessions } => Some(sessions.clone()),
                _ => None,
            })
            .expect("a SessionList event must be emitted even with no catalog");
        assert!(
            sessions.is_empty(),
            "no on-disk catalog must yield an empty SessionList, got {sessions:?}"
        );
    });
}

/// SESSIONS/HISTORY: `submit(NewSession)` clears the session (minting a fresh
/// id) and confirms with a `SessionStarted` carrying the new connection
/// session id — proving the command drives the real orchestrator handle, not
/// the no-op catch-all. The reported id matches the orchestrator's
/// `current_session_id` after the swap.
#[test]
fn submit_new_session_emits_session_started() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        use lingxi_core::host::OrchestratorHandle;
        let oh: Arc<dyn OrchestratorHandle> = handle.inner().orchestrator.clone();
        let before = oh.current_session_id().await.to_string();

        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .expect("submit(NewSession) ok");

        let after = oh.current_session_id().await.to_string();
        let after_uuid = oh.current_session_id().await.as_uuid().to_string();
        assert_ne!(before, after, "NewSession must mint a fresh session id");

        let events = drained(&listener).await;
        let started = events.iter().find_map(|e| match e {
            Ev::SessionStarted { session_id, mode } => Some((session_id.clone(), *mode)),
            _ => None,
        });
        assert_eq!(
            started.expect("a SessionStarted event must be emitted"),
            (after_uuid.clone(), SessionModeDto::Code),
            "SessionStarted must carry the bare resumable UUID"
        );

        let path =
            session::jsonl::session_path(&handle.lingxi_home, &handle.session_cwd(), &after_uuid);
        let raw = std::fs::read_to_string(path).expect("new session anchor exists");
        let records = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSONL"))
            .collect::<Vec<_>>();
        let anchor = records
            .iter()
            .find(|record| record["mobileEmptySession"] == 1)
            .expect("new session anchor exists");
        assert_eq!(anchor["sessionId"], after_uuid);
        assert_eq!(anchor["mobileEmptySession"], 1);
        let expected_permission_mode = handle.inner().session_default_permission_mode.clone();
        assert!(records.iter().any(|record| {
            record["type"] == "permission-mode"
                && record["sessionId"] == after_uuid
                && record["permissionMode"] == expected_permission_mode
        }));
        assert!(records.iter().any(|record| {
            record["type"] == "session-mode"
                && record["sessionId"] == after_uuid
                && record["sessionMode"] == "code"
        }));

        handle
            .submit(ClientCommand::ListSessions { limit: None })
            .await
            .expect("list anchored empty session");
        let events = drained(&listener).await;
        let row = events.iter().rev().find_map(|event| match event {
            Ev::SessionList { sessions } => {
                sessions.iter().find(|row| row.uuid == after_uuid).cloned()
            }
            _ => None,
        });
        let row = row.expect("anchored empty session must be listed");
        assert_eq!(row.message_count, 0);
        assert_eq!(row.mode, SessionModeDto::Code);
    });
}

#[test]
fn submit_resume_session_restores_anchored_empty_session_with_same_uuid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        use lingxi_core::host::OrchestratorHandle;

        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .expect("create anchored empty session");
        let expected = handle
            .inner()
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string();

        handle
            .submit(ClientCommand::ResumeSession {
                session_id: expected.clone(),
                cwd: None,
            })
            .await
            .expect("anchored empty session resumes");

        assert_eq!(
            handle
                .inner()
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string(),
            expected,
        );
        let events = drained(&listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::SessionResumed {
                session_id,
                mode,
                messages,
            } if session_id == &expected && mode == &SessionModeDto::Code && messages.is_empty()
        )));
    });
}

#[test]
fn submit_resume_session_restores_its_persisted_permission_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (session_id, _) = seed_replay_valid_session(tmp.path());
    let cfg = test_config(tmp.path());
    let path =
        session::jsonl::session_path(&cfg.lingxi_home, &cfg.cwd.to_string_lossy(), &session_id);
    let record = serde_json::json!({
        "type": "permission-mode",
        "permissionMode": "bypassPermissions",
        "sessionId": session_id,
    });
    let mut transcript = std::fs::read_to_string(&path).expect("read seeded transcript");
    transcript.push_str(&format!("{record}\n"));
    std::fs::write(&path, transcript).expect("append permission mode");

    let (handle, _) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        use lingxi_core::host::OrchestratorHandle;

        handle
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .expect("resume persisted session");
        let orchestrator: Arc<dyn OrchestratorHandle> = handle.inner().orchestrator.clone();
        assert_eq!(
            orchestrator.permission_mode().await.as_deref(),
            Some("bypassPermissions")
        );
    });
}

#[test]
fn submit_resume_session_rejects_a_session_from_another_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (session_id, _) = seed_replay_valid_session(tmp.path());
    let cfg = test_config(tmp.path());
    let path =
        session::jsonl::session_path(&cfg.lingxi_home, &cfg.cwd.to_string_lossy(), &session_id);
    let mut transcript = std::fs::read_to_string(&path).expect("read seeded transcript");
    transcript.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "session-mode",
            "sessionMode": "chat",
            "sessionId": session_id,
        })
    ));
    std::fs::write(&path, transcript).expect("append session mode");

    let code_cfg = test_config(tmp.path());
    let (handle, listener) = build_submit_handle_with_config(code_cfg, tmp.path());
    handle.runtime().block_on(async {
        let active_before = handle.inner().orchestrator.current_session_id().await;
        let result = handle
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await;
        assert!(
            matches!(result, Err(ClientError::Rejected { ref message })
                    if message.contains("belongs to chat mode")
                        && message.contains("runs code mode")),
            "{result:?}"
        );
        let events = drained(&listener).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ev::SessionResumed { .. })),
            "a rejected cross-mode resume must not emit SessionResumed"
        );
        assert_eq!(
            handle.inner().orchestrator.current_session_id().await,
            active_before,
            "mode validation must happen before mutating the active session"
        );
    });
}

#[test]
fn submit_resume_session_rejects_legacy_mode_less_session_from_chat_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (session_id, _) = seed_replay_valid_session(tmp.path());
    let mut chat_cfg = test_config(tmp.path());
    chat_cfg.session_mode = session::jsonl::SessionMode::Chat;
    let (handle, listener) = build_submit_handle_with_config(chat_cfg, tmp.path());

    handle.runtime().block_on(async {
        let active_before = handle.inner().orchestrator.current_session_id().await;
        let result = handle
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await;
        assert!(
            matches!(result, Err(ClientError::Rejected { ref message })
                    if message.contains("belongs to code mode")
                        && message.contains("runs chat mode")),
            "{result:?}"
        );
        let events = drained(&listener).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ev::SessionResumed { .. })),
            "a rejected legacy code resume must not emit SessionResumed"
        );
        assert_eq!(
            handle.inner().orchestrator.current_session_id().await,
            active_before,
            "legacy fallback validation must not disturb the active Chat session"
        );
    });
}

#[test]
fn submit_fork_session_copies_context_into_target_mode_without_mutating_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (source_session_id, visible_message_count) = seed_replay_valid_session(tmp.path());
    let cfg = test_config(tmp.path());
    let cwd = cfg.cwd.to_string_lossy().into_owned();
    let source_path = session::jsonl::session_path(&cfg.lingxi_home, &cwd, &source_session_id);
    let source_before = std::fs::read_to_string(&source_path).expect("read source transcript");
    let (handle, listener) = build_submit_handle_with_config(cfg.clone(), tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ForkSession {
                session_id: source_session_id.clone(),
                target_mode: SessionModeDto::Chat,
            })
            .await
            .expect("fork session into Chat");

        let events = drained(&listener).await;
        let fork_session_id = events
            .iter()
            .find_map(|event| match event {
                Ev::SessionForked {
                    source_session_id: reported_source,
                    session_id,
                    mode,
                } if reported_source == &source_session_id && *mode == SessionModeDto::Chat => {
                    Some(session_id.clone())
                }
                _ => None,
            })
            .expect("SessionForked event");

        let source_after =
            std::fs::read_to_string(&source_path).expect("read unchanged source transcript");
        assert_eq!(
            source_after, source_before,
            "fork must not mutate its source"
        );

        let fork_path = session::jsonl::session_path(&cfg.lingxi_home, &cwd, &fork_session_id);
        let fork_raw = std::fs::read_to_string(&fork_path).expect("read fork transcript");
        assert!(fork_raw.contains("resume me from disk"));
        assert!(fork_raw.contains("resumed!"));
        assert!(fork_raw.contains("\"sessionMode\":\"chat\""));

        let rows = session::jsonl::list_recent_sessions(
            &cfg.lingxi_home,
            &cwd,
            usize::MAX,
            handle.fs.clone(),
        )
        .await
        .expect("list forked session");
        let fork_row = rows
            .iter()
            .find(|row| row.uuid.to_string() == fork_session_id)
            .expect("fork row");
        assert_eq!(fork_row.mode, session::jsonl::SessionMode::Chat);
        assert_eq!(fork_row.message_count, visible_message_count);
    });
}

#[test]
fn chat_source_rejects_a_code_only_slash_command_before_dispatch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut chat_cfg = test_config(tmp.path());
    chat_cfg.session_mode = session::jsonl::SessionMode::Chat;
    let (handle, listener) = build_submit_handle_with_config(chat_cfg, tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::RunSlashCommand {
                raw: "/loop every 5 minutes inspect the workspace".into(),
                images: Vec::new(),
                turn_id: Some(42),
            })
            .await
            .expect("Chat rejection is a handled command result");

        let events = drained(&listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::SlashCommandResult {
                turn_id: Some(42),
                display,
                is_error: true,
            } if display.display().contains("unavailable in Chat mode")
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ev::TurnStarted { .. })),
            "a hidden Code-only command must never reach turn dispatch"
        );
    });
}

#[test]
fn resume_empty_session_bootstraps_legacy_project_index_uuid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());
    let expected = "dddddddd-4444-4444-8444-dddddddddddd";

    handle.runtime().block_on(async {
        use lingxi_core::host::OrchestratorHandle;

        handle
            .resume_empty_session(expected.into(), "旧空会话".into())
            .await
            .expect("legacy indexed empty session resumes");

        assert_eq!(
            handle
                .inner()
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string(),
            expected,
        );
        let path =
            session::jsonl::session_path(&handle.lingxi_home, &handle.session_cwd(), expected);
        let raw = std::fs::read_to_string(path).expect("migration anchor exists");
        assert!(raw.contains("\"mobileEmptySession\":1"));
        assert!(raw.contains("旧空会话"));

        let events = drained(&listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::SessionResumed {
                session_id,
                mode,
                messages,
            } if session_id == expected && mode == &SessionModeDto::Code && messages.is_empty()
        )));
    });
}

/// Seed a REPLAY-VALID session JSONL under
/// `<lingxi_home>/projects/<sanitize(cwd)>/<uuid>.jsonl` — a user+assistant
/// pair with a proper `parentUuid` chain (first msg parent=null, the second's
/// parent = the first's uuid, both `sessionId == <file uuid>`) so it PASSES
/// the loader's `validate_chain`. Returns `(file_uuid, user_assistant_count)`.
fn seed_replay_valid_session(root: &std::path::Path) -> (String, usize) {
    let cfg = test_config(root);
    let cwd = cfg.cwd.to_string_lossy().into_owned();
    let project_dir = cfg
        .lingxi_home
        .join("projects")
        .join(session::jsonl::project_dir_name(&cwd));
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    // Fixed, valid UUID literals (harness-runtime::mobile parses, never mints, in the
    // test). The file stem IS the sessionId; the two messages carry DISTINCT
    // `uuid`s forming a one-link parent chain.
    let file_uuid = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".to_string();
    let user_uuid = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb".to_string();
    let asst_uuid = "cccccccc-3333-4333-8333-cccccccccccc".to_string();
    let path = project_dir.join(format!("{file_uuid}.jsonl"));

    let user_line = serde_json::json!({
        "type": "user",
        "uuid": user_uuid,
        "parentUuid": serde_json::Value::Null,
        "sessionId": file_uuid,
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": "resume me from disk"}
    });
    let asst_line = serde_json::json!({
        "type": "assistant",
        "uuid": asst_uuid,
        "parentUuid": user_uuid,
        "sessionId": file_uuid,
        "timestamp": "2026-05-25T12:00:01.000Z",
        "cwd": cwd,
        "version": "0.6.0",
        "isSidechain": false,
        "message": {"role": "assistant", "content": [{"type": "text", "text": "resumed!"}]}
    });
    let body = format!(
        "{}\n{}\n",
        serde_json::to_string(&user_line).unwrap(),
        serde_json::to_string(&asst_line).unwrap()
    );
    std::fs::write(&path, body).expect("write replay-valid session file");
    (file_uuid, 2)
}

/// SESSIONS/HISTORY (live ResumeSession): `submit(ResumeSession)` against a
/// REPLAY-VALID on-disk session hot-restores it into the running orchestrator.
/// Asserts a `SessionResumed` event is emitted whose `messages.len()` equals
/// the seeded user+assistant count, AND the orchestrator's live
/// `conversation_transcript` equals the restored history (proving the model
/// will see prior context on the next turn).
#[test]
fn submit_resume_session_rehydrates_and_emits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (file_uuid, seeded_count) = seed_replay_valid_session(tmp.path());
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        use lingxi_core::host::OrchestratorHandle;

        let result = handle
            .submit(ClientCommand::ResumeSession {
                session_id: file_uuid.clone(),
                cwd: None,
            })
            .await;
        assert!(
            result.is_ok(),
            "ResumeSession against a replay-valid session must succeed, got {result:?}"
        );

        // A SessionResumed carrying the full restored transcript was emitted.
        let events = drained(&listener).await;
        let messages = events
            .iter()
            .find_map(|e| match e {
                Ev::SessionResumed {
                    session_id,
                    mode,
                    messages,
                } => {
                    assert_eq!(
                        session_id, &file_uuid,
                        "resumed id must be the named session"
                    );
                    assert_eq!(*mode, SessionModeDto::Code);
                    Some(messages.clone())
                }
                _ => None,
            })
            .expect("a SessionResumed event must be emitted on a successful resume");
        assert_eq!(
            messages.len(),
            seeded_count,
            "SessionResumed.messages must carry every replayed user/assistant message"
        );

        // The RUNNING orchestrator adopted the restored history — the next
        // turn will see the prior context.
        let oh: Arc<dyn OrchestratorHandle> = handle.inner().orchestrator.clone();
        let transcript = oh.conversation_transcript().await;
        assert_eq!(
            transcript.len(),
            seeded_count,
            "the live orchestrator must hold the restored transcript after resume"
        );
        // The adopted id is the named session (resume does NOT mint a fresh one).
        assert_eq!(
            oh.current_session_id().await.as_uuid().to_string(),
            file_uuid,
            "resume must adopt the named session id on the live orchestrator"
        );
    });
}

/// Older Android builds persisted `SessionId::Display` (`sess:<uuid>`) in
/// their Project session index. The mobile resume boundary accepts that
/// legacy spelling once, but emits the canonical bare UUID so the client can
/// rewrite its cache without carrying the prefix forward.
#[test]
fn submit_resume_session_accepts_legacy_display_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (file_uuid, seeded_count) = seed_replay_valid_session(tmp.path());
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ResumeSession {
                session_id: format!("sess:{file_uuid}"),
                cwd: None,
            })
            .await
            .expect("legacy prefixed session id must resume");

        let events = drained(&listener).await;
        let resumed = events.iter().find_map(|event| match event {
            Ev::SessionResumed {
                session_id,
                mode,
                messages,
            } => Some((session_id.clone(), *mode, messages.len())),
            _ => None,
        });
        assert_eq!(
            resumed,
            Some((file_uuid, SessionModeDto::Code, seeded_count)),
            "legacy input must be confirmed with a bare UUID"
        );
    });
}

/// SESSIONS/HISTORY (live ResumeSession): an UNKNOWN session uuid (no on-disk
/// file) is honestly REJECTED — a missing session is genuinely not resumable,
/// so we return `ClientError::Rejected` rather than emit a false `SessionResumed`.
#[test]
fn submit_resume_session_missing_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let result = handle
            .submit(ClientCommand::ResumeSession {
                // A well-formed uuid that names no on-disk session.
                session_id: "dddddddd-4444-4444-8444-dddddddddddd".into(),
                cwd: None,
            })
            .await;
        assert!(
            matches!(result, Err(ClientError::Rejected { .. })),
            "an unknown session must be Rejected, got {result:?}"
        );
        let events = drained(&listener).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ev::SessionResumed { .. })),
            "a rejected ResumeSession must NOT emit a (false) SessionResumed event"
        );
    });
}

/// SESSIONS/HISTORY (live ResumeSession): a MALFORMED session id (not a uuid)
/// is honestly REJECTED — we parse the id as a `Uuid` first and reject a
/// non-uuid rather than fake a confirmation.
#[test]
fn submit_resume_session_malformed_id_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let result = handle
            .submit(ClientCommand::ResumeSession {
                session_id: "not-a-uuid".into(),
                cwd: None,
            })
            .await;
        assert!(
            matches!(result, Err(ClientError::Rejected { .. })),
            "a malformed session id must be Rejected, got {result:?}"
        );
        let events = drained(&listener).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ev::SessionResumed { .. })),
            "a rejected ResumeSession must NOT emit a SessionResumed event"
        );
    });
}

/// SESSIONS/HISTORY (live ResumeSession): resume is REJECTED while a turn is in
/// flight (mirror of `ClearSession` / `NewSession` mid-turn guards). We arm a
/// live (un-cancelled) cancel token via `SendPrompt`, then submit Resume.
#[test]
fn submit_resume_session_mid_turn_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (file_uuid, _count) = seed_replay_valid_session(tmp.path());
    let (handle, _listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        // Arm an in-flight turn so a live cancel token is recorded.
        handle
            .submit(ClientCommand::SendPrompt {
                text: "drive a turn".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: None,
                visualization_context: None,
            })
            .await
            .expect("submit(SendPrompt) ok");
        assert!(
            !handle.active_turn_is_cancelled().await,
            "the freshly-armed turn token must not be cancelled yet"
        );

        let result = handle
            .submit(ClientCommand::ResumeSession {
                session_id: file_uuid,
                cwd: None,
            })
            .await;
        assert!(
            matches!(result, Err(ClientError::Rejected { .. })),
            "resume must be Rejected while a turn is in flight, got {result:?}"
        );
    });
}

// ── TPM-C (mobile): default_model profile/model parsing ──────────────────

/// Verifies the listings-building + `parse_model_ref` logic the mobile
/// composition root uses at build time: a qualified `profile/model`
/// default_model splits into the bare id (written to `orch_cfg.model`) and
/// `Some(profile)` (used to seed `switch_model`), while a bare id passes
/// through unchanged with `None` profile (no-op seed path).
///
/// This is a pure unit test of the parser + listing shape — no I/O, no
/// tokio runtime — mirroring `default_model_parse_qualified_and_bare` in
/// harness-runtime::desktop.
#[test]
fn mobile_default_model_parse_qualified_and_bare() {
    // Construct the same listing shape the mobile composition root builds
    // from `assembled.client_config.providers` (display_model == request_model
    // on mobile; provider_label == profile_name).
    let listings = vec![
        lingxi_core::host::ModelListing {
            connection: Default::default(),
            display_model: "gpt-5.2".to_string(),
            request_model: "gpt-5.2".to_string(),
            provider_id: "openai".to_string(),
            provider_label: "openai".to_string(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: false,
            fusion_analyst_capable: false,
        },
        lingxi_core::host::ModelListing {
            connection: Default::default(),
            display_model: "gpt-5.2".to_string(),
            request_model: "gpt-5.2".to_string(),
            provider_id: "github-copilot".to_string(),
            provider_label: "github-copilot".to_string(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: false,
            fusion_analyst_capable: false,
        },
        lingxi_core::host::ModelListing {
            connection: Default::default(),
            display_model: "claude-sonnet-4-20250514".to_string(),
            request_model: "claude-sonnet-4-20250514".to_string(),
            provider_id: "anthropic".to_string(),
            provider_label: "anthropic".to_string(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: true,
            fusion_analyst_capable: false,
        },
    ];

    // Qualified: "openai/gpt-5.2" → bare id "gpt-5.2" + profile "openai"
    let (id, profile) = lingxi_core::host::parse_model_ref("openai/gpt-5.2", &listings);
    assert_eq!(id, "gpt-5.2", "qualified ref must strip the profile prefix");
    assert_eq!(
        profile.as_deref(),
        Some("openai"),
        "qualified ref must extract the profile"
    );

    // Bare: "claude-sonnet-4-20250514" → same id, no profile (no-op seed path)
    let (id2, profile2) = lingxi_core::host::parse_model_ref("claude-sonnet-4-20250514", &listings);
    assert_eq!(
        id2, "claude-sonnet-4-20250514",
        "bare model id must pass through"
    );
    assert!(profile2.is_none(), "bare model must yield None profile");

    // Shared id with two providers and explicit profile qualifier
    let (id3, profile3) = lingxi_core::host::parse_model_ref("github-copilot/gpt-5.2", &listings);
    assert_eq!(id3, "gpt-5.2");
    assert_eq!(profile3.as_deref(), Some("github-copilot"));
}

#[test]
fn mobile_model_refs_keep_duplicate_provider_models_distinct() {
    let listings = vec![
        lingxi_core::host::ModelListing {
            connection: Default::default(),
            display_model: "gpt-5.6-sol".into(),
            request_model: "gpt-5.6-sol".into(),
            provider_id: "openai".into(),
            provider_label: "OpenAI".into(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: true,
            fusion_analyst_capable: false,
        },
        lingxi_core::host::ModelListing {
            connection: Default::default(),
            display_model: "gpt-5.6-sol".into(),
            request_model: "gpt-5.6-sol".into(),
            provider_id: "github-copilot".into(),
            provider_label: "GitHub Copilot".into(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: true,
            fusion_analyst_capable: false,
        },
    ];

    let refs = lingxi_core::host::curated_model_refs(
        &listings,
        &["gpt-5.6-sol".into()],
        "gpt-5.6-sol",
        Some("github-copilot"),
    );

    assert_eq!(refs[0], "github-copilot/gpt-5.6-sol");
    assert!(refs.iter().any(|model| model == "openai/gpt-5.6-sol"));
    assert_eq!(
        refs.iter()
            .filter(|model| model.as_str() == "github-copilot/gpt-5.6-sol")
            .count(),
        1,
        "the active model and catalog row must de-duplicate by qualified id"
    );
    assert!(
        !refs.iter().any(|model| model == "gpt-5.6-sol"),
        "ambiguous bare ids must not leak into the mobile picker"
    );
}

/// Drain and return every event delivered to the fake listener so far.
/// Event delivery is asynchronous, in two ordered stages: `AppService`
/// hands events to spawned emission tasks (commit → enqueue onto the
/// bridge's emission channel), and the bridge's single forwarder drains
/// that channel to the sink (enqueue → deliver). A bare take races both,
/// so the barrier is two-stage too: `flush_events` waits until everything
/// committed is ENQUEUED, then the queue flush waits until everything
/// enqueued is DELIVERED.
async fn drain_events(_handle: &MobileEngineHandle, listener: &FakeListener) -> Vec<Ev> {
    std::mem::take(&mut *listener.received.lock().await)
}

/// r1-engine-core-002: a filesystem double that fails the Nth
/// `append_file_with_mode` call whose path ends with `target_suffix`,
/// delegating every other call (and every other trait method) to a real
/// `PosixFileSystem`. Used to force `mint_app_init_session`'s SECOND
/// write (the `session-mode` metadata append that runs after the
/// anchor/fork content has already landed) to fail, so the gate below
/// can prove the first write's bytes get cleaned up rather than
/// stranded.
struct FlakyAppendFs {
    inner: Arc<dyn lingxi_core::host::FileSystem>,
    target_suffix: String,
    calls: std::sync::atomic::AtomicUsize,
    fail_on_call: usize,
}

#[async_trait]
impl lingxi_core::host::FileSystem for FlakyAppendFs {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<lingxi_core::host::FileContent, lingxi_core::host::FsError> {
        self.inner.read_file(path, offset, limit).await
    }

    async fn write_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), lingxi_core::host::FsError> {
        self.inner.write_file(path, content).await
    }

    fn is_within_workspace(&self, path: &str) -> bool {
        self.inner.is_within_workspace(path)
    }

    async fn watch(
        &self,
        dir: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::stream::Stream<Item = lingxi_core::host::FileEvent> + Send>,
        >,
        lingxi_core::host::FsError,
    > {
        self.inner.watch(dir).await
    }

    async fn append_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), lingxi_core::host::FsError> {
        self.inner.append_file(path, content).await
    }

    async fn append_file_with_mode(
        &self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), lingxi_core::host::FsError> {
        if path.ends_with(&self.target_suffix) {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == self.fail_on_call {
                return Err(lingxi_core::host::FsError::Io(
                    "r1-engine-core-002 planted failure".into(),
                ));
            }
        }
        self.inner.append_file_with_mode(path, content, mode).await
    }

    async fn truncate(&self, path: &str, len: u64) -> Result<(), lingxi_core::host::FsError> {
        self.inner.truncate(path, len).await
    }

    async fn file_mtime(
        &self,
        path: &str,
    ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
        self.inner.file_mtime(path).await
    }

    async fn file_size(&self, path: &str) -> Result<u64, lingxi_core::host::FsError> {
        self.inner.file_size(path).await
    }

    async fn delete_file(&self, path: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.delete_file(path).await
    }

    async fn symlink(&self, target: &str, link: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.symlink(target, link).await
    }

    async fn flock_exclusive(
        &self,
        path: &str,
    ) -> Result<Box<dyn lingxi_core::host::FlockGuard>, lingxi_core::host::FsError> {
        self.inner.flock_exclusive(path).await
    }

    async fn fsync(&self, path: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.fsync(path).await
    }
}

/// New sessions may choose another directory; invalid paths leave the owner intact.
#[test]
fn new_session_accepts_another_existing_directory_and_rejects_missing_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(tmp.path());
    let before = handle.runtime().block_on(handle.workspace_state());
    let other = tmp.path().join("another-worktree");
    std::fs::create_dir_all(&other).unwrap();
    handle.runtime().block_on(async {
        assert!(handle
            .submit(ClientCommand::NewSession {
                cwd: Some(tmp.path().join("missing").display().to_string()),
                model: None
            })
            .await
            .is_err());
        assert_eq!(handle.workspace_state().await, before);
        handle
            .submit(ClientCommand::NewSession {
                cwd: Some(other.display().to_string()),
                model: None,
            })
            .await
            .unwrap();
    });
    let after = handle.runtime().block_on(handle.workspace_state());
    assert_ne!(after.session_id, before.session_id);
    assert_eq!(Path::new(&after.cwd), other.canonicalize().unwrap());
    assert_eq!(after.project_root, after.cwd);
}

#[cfg(unix)]
#[test]
fn mobile_prompt_rejects_foreign_transcript_symlink_without_mutation() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("mobile transcript root");
    let foreign_root = tempfile::tempdir().expect("foreign transcript root");
    let (handle, _listener) = build_submit_handle(tmp.path());
    let transcript_path = handle.inner.session_writer.active_path();
    std::fs::create_dir_all(
        transcript_path
            .parent()
            .expect("mobile transcript has a parent"),
    )
    .expect("create mobile transcript directory");
    let foreign_path = foreign_root.path().join("foreign-session.jsonl");
    let foreign_bytes = b"foreign transcript must remain byte-for-byte unchanged\n";
    std::fs::write(&foreign_path, foreign_bytes).expect("write foreign transcript fixture");
    symlink(&foreign_path, &transcript_path).expect("link active transcript to foreign file");

    let error = handle
        .runtime()
        .block_on(handle.submit(ClientCommand::SendPrompt {
            text: ClientTextDto::from_code_units("must not append".encode_utf16().collect()),
            prompt_mode: None,
            images: Vec::new(),
            turn_id: Some(248),
            visualization_context: None,
        }))
        .expect_err("Host must reject a symlinked Native transcript before turn execution");
    assert!(
        matches!(&error, ClientError::Internal { message }
            if message.contains("transcript is not a regular file")),
        "unexpected transcript rejection: {error:?}"
    );
    assert_eq!(
        std::fs::read(&foreign_path).expect("read foreign transcript after rejection"),
        foreign_bytes,
        "a rejected foreign transcript is never rewritten"
    );
    let mut sidecar_name = transcript_path
        .file_name()
        .expect("mobile transcript has a filename")
        .to_os_string();
    sidecar_name.push(branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX);
    assert!(
        !transcript_path.with_file_name(sidecar_name).exists(),
        "rejection must not create an identity sidecar beside the foreign path"
    );
    let reservation_released = handle
        .runtime()
        .block_on(async { handle.active_cancel.lock().await.is_none() });
    assert!(
        reservation_released,
        "identity import rejection must release the reserved turn"
    );
}

/// Index of the first event matching `pred`, or a panic naming what was
/// expected and the whole batch. The app surface delivers through ONE
/// ordered channel (channel order = commit order), so multi-event batches
/// assert relative POSITIONS, not mere membership (W5).
fn position_of(events: &[Ev], what: &str, pred: impl Fn(&Ev) -> bool) -> usize {
    events
        .iter()
        .position(pred)
        .unwrap_or_else(|| panic!("{what} not found in {events:?}"))
}

// ── Create-flow: `CreateApp{mode}` forks shell vs scaffolded ───────────

#[test]
fn mobile_handback_admission_wakes_a_real_model_turn_with_peer_meta_and_stable_id() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use lingxi_core::host::handback::*;
    use lingxi_core::host::task_registry::{TaskCreateInput, TaskRegistryHandle};
    use orchestrator::test_support_stream::*;
    let tmp = tempfile::tempdir().unwrap();
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("handback-mobile", "test"),
        content_block_start_text(0),
        text_delta(0, "report received"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        assert!(handle.inner.session_writer.durable_transcript_enabled(),
            "production mobile assembly installs a durable transcript authority");
        handle.inner.orchestrator.recover_main_reports().await
            .expect("actual mobile report inbox must recover with its durable writer");
        let registry = handle.inner.task_registry.as_ref();
        let scope = registry.handback_scope().await.expect("actual mobile reporting admission is bound");
        assert_eq!(scope.session_id, handle.inner.orchestrator.current_session_id().await);
        let task = TaskRegistryHandle::create(registry, TaskCreateInput {
            task_type: "local_agent".into(), description: "reporting child".into(),
        }).await.unwrap();
        let sender = lingxi_core::types::AgentId::new();
        registry.bind_agent_id(&task.task_id, sender).await.unwrap();
        TaskRegistryHandle::set_status(registry, &task.task_id, "running").await.unwrap();
        let token = registry.begin_handback_run(BeginHandbackRun {
            agent_id: sender, scope, active: true, caller: None,
            resumer: HandbackRecipient::Main { scope }, restored_state: None,
            restored_history: Vec::new(),
        }).await.unwrap();
        let message_id = lingxi_core::types::MessageId::new();
        let body = handback_frame("/clear\npeer output carries no approval authority");
        let delivered = registry.try_deliver_handback(&token, PreparedHandbackReport {
            message_id, report: HandbackReport::from_utf8_text("whole report", None),
            body: lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                body.clone(), body.encode_utf16().collect(),
            ).unwrap(), sender_name: "reporter".into(), sender_id: sender.to_string(), sender_task_id: task.task_id.clone(),
            agent_type: "general-purpose".into(), flagged: false,
        }).await;
        assert!(matches!(delivered, HandbackAdmissionOutcome::Admitted(_)));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !streaming.captured_calls().await.is_empty() && handle.active_cancel.lock().await.is_none() { break }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let calls = streaming.captured_calls().await;
        assert_eq!(calls.len(), 1, "report queue must run the idle model turn");
        let report_message = calls[0].messages.iter().find(|message| {
            matches!(message, lingxi_core::types::ConversationMessage::User { id, is_meta: true, .. } if *id == message_id)
        }).expect("stable admitted MessageId reaches the model as meta");
        assert!(serde_json::to_string(report_message).unwrap().contains("no approval authority"));
        assert_eq!(handle.inner.orchestrator.current_session_id().await, scope.session_id, "peer /clear is not a slash command");
        assert!(!handle.inner.orchestrator.has_pending_main_reports(scope).await);
        let transcript = tokio::fs::read_to_string(orchestrator::transcript_paths::main_transcript_path(
            &handle.lingxi_home, &handle.session_cwd(), &scope.session_id.as_uuid().to_string(),
        )).await.unwrap();
        assert!(transcript.contains("reporter") && transcript.contains("peer"), "typed report provenance must persist with its model-visible row");
    });
}

#[test]
fn mobile_transcript_authority_follows_real_new_and_resume_session_activation() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        let original = handle.inner.orchestrator.current_session_id().await;
        let original_path = handle
            .inner
            .session_writer
            .session_target_path(original)
            .unwrap();
        handle
            .inner
            .session_writer
            .append_mobile_empty_session(
                &original.as_uuid().to_string(),
                "original conversation",
                &handle.session_cwd(),
            )
            .await
            .unwrap();
        handle
            .inner
            .session_writer
            .append_session_mode(handle.inner.session_mode.as_str())
            .await
            .unwrap();
        let original_scope = handle.inner.task_registry.handback_scope().await.unwrap();

        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        let next = handle.inner.orchestrator.current_session_id().await;
        assert_ne!(next, original);
        let next_scope = handle.inner.task_registry.handback_scope().await.unwrap();
        assert_eq!(next_scope.session_id, next);
        let next_path = handle
            .inner
            .session_writer
            .session_target_path(next)
            .unwrap();
        assert_ne!(next_path, original_path);
        assert_eq!(handle.inner.session_writer.active_path(), next_path);
        assert_eq!(
            handle.inner.session_writer.session_target_path(original),
            Some(original_path.clone())
        );

        handle
            .submit(ClientCommand::ResumeSession {
                session_id: original.as_uuid().to_string(),
                cwd: None,
            })
            .await
            .unwrap();
        let resumed_scope = handle.inner.task_registry.handback_scope().await.unwrap();
        assert_eq!(resumed_scope.session_id, original);
        assert!(resumed_scope.activation_epoch > original_scope.activation_epoch);
        assert_eq!(handle.inner.session_writer.active_path(), original_path);
        assert_eq!(
            handle.inner.session_writer.session_target_path(next),
            Some(next_path)
        );
    });
}

/// Exercise the production `/context` failure path through the mobile submit
/// queue. A settings-source merge error must be returned as a local slash
/// result, durably delivered, and never start an ordinary model request.
#[test]
fn mobile_context_settings_failure_delivers_and_settles_durable_slash_without_model_request() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("mobile context root");
    let mut cfg = test_config(root.path());
    // Keep the User settings layer distinct from the project settings layer.
    cfg.lingxi_home = root.path().join("user-settings-home");
    let user_settings = cfg.lingxi_home.join("settings.json");
    let project_settings = cfg.cwd.join(".lingxi/settings.json");
    let listener = Arc::new(FakeListener::default());
    let streaming = Arc::new(MockStreamingApiClient::with_open_error(
        llm_runtime::LlmError::Authentication {
            message: String::new(),
        },
        Vec::new(),
    ));
    let handle = new_engine_with_streaming(
        cfg,
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .expect("build production mobile host with deterministic streaming client");

    handle.runtime().block_on(async {
        std::fs::create_dir_all(user_settings.parent().expect("user settings parent"))
            .expect("create user settings home");
        std::fs::write(&user_settings, r#"{"extraKnownMarketplaces":[]}"#)
            .expect("write lower array source");
        std::fs::create_dir_all(project_settings.parent().expect("project settings parent"))
            .expect("create project settings directory");
        std::fs::write(
            &project_settings,
            r#"{"extraKnownMarketplaces":{"length":1.5}}"#,
        )
        .expect("write invalid Native array-length override");

        // Confirm the real, wired settings reader sees the fixture error before
        // exercising the public slash-command path that consumes it.
        let settings_error = handle
            .inner
            .wired_skill_listing_provider
            .skill_listing_snapshot()
            .await
            .expect_err("the two real settings layers must fail their Native merge");
        assert_eq!(
            settings_error.to_string(),
            "RangeError: Invalid array length"
        );

        handle
            .submit(ClientCommand::RunSlashCommand {
                raw: "/context".into(),
                images: Vec::new(),
                turn_id: Some(641),
            })
            .await
            .expect("the failed local slash is acknowledged");

        let events = listener.received.lock().await.clone();
        let exact_message = "RangeError: Invalid array length"
            .encode_utf16()
            .collect::<Vec<_>>();
        assert!(
            events.iter().any(|event| matches!(
                event,
                Ev::SlashCommandResult {
                    turn_id: Some(641),
                    display,
                    is_error: true,
                } if display.code_units == exact_message
            )),
            "the production /context failure must reach the client unchanged: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ev::TurnStarted { .. })),
            "a local Failed slash must not enter ordinary turn lifecycle: {events:?}"
        );
        assert!(
            streaming.captured_calls().await.is_empty(),
            "a local Failed slash must not issue a model request"
        );

        let session_id = handle
            .inner
            .active_session_uuid
            .lock()
            .expect("active session identity")
            .clone();
        let checkpoint = handle
            .durable_turns
            .load(&session_id, 641)
            .expect("read the settled durable slash checkpoint");
        assert_eq!(
            checkpoint.queue_operation,
            crate::mobile::turn_durability::QueueOperationKind::Slash
        );
        assert_eq!(checkpoint.queue_delivery, QueueDeliveryState::Delivered);
        assert!(checkpoint.state.is_none());
        assert!(!checkpoint.safe_to_resume);
        assert_eq!(checkpoint.reason, None);
        assert!(
            handle.active_cancel.lock().await.is_none(),
            "the local slash owner must be released after durable delivery"
        );
    });
}
