include!("../fast_mode_preference_tests.rs");
include!("../permission_preference_tests.rs");
include!("../model_preference_tests.rs");
include!("../provider_region_tests.rs");
include!("../reasoning_preference_tests.rs");
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::mobile::local_apps_mcp::LocalAppsMcpTransport;
use async_trait::async_trait;
use client::adapter::{ClientEventListener, ListenerSink, MockSink, PermissionRequestSink};
use client::protocol::events::ClientEvent;
use client::protocol::listings::SessionModeDto;
use lingxi_core::host::subagent_spawn::{SubagentObservation, SubagentSpawnObserver};
use lingxi_core::host::{
    OrchestratorHandle as _, SlashCommandDispatcher as _, SlashDispatchResult,
};
use tokio::sync::Notify;
use tool_skill::skill::SkillCommandType;

use super::{
    build_mobile, builtin_provider_catalog, classify_provider_connection_response,
    collect_session_agent_transcript_paths, find_session_agent_transcript_path,
    lower_session_agent_snapshot, mobile_cron_schedule_error, mobile_mcp_config_unchanged,
    mobile_mcp_oauth_authorization_callback, mobile_mcp_preflight, mobile_mcp_record_reload_intent,
    mobile_mcp_reload_requires_replacement, mobile_mcp_run_reload_job,
    mobile_mcp_state_is_transitional, mobile_skill_listing_provider,
    session_agent_conversation_is_visible, session_agent_transcript_event,
    session_agent_transcript_revision, McpConfigScope, McpRegistry, McpServerConfig, MobileConfig,
    MobileCronStoreHandle, MobileMcpReloadJob, MobileRuntime, MobileSessionAgentObserver,
};

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
        assert_eq!(entry.display_name, provider.profile_name);
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
    let input = serde_json::json!({
        "scriptPath": script_path.to_string_lossy().into_owned()
    });
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
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "nested child".to_string(),
                }],
                stop_reason: None,
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
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "visible".to_string(),
        }],
        stop_reason: None,
    };
    let compact = lingxi_core::types::ConversationMessage::User {
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "replacement summary".to_string(),
        }],
        is_meta: false,
        is_compact_summary: true,
        is_visible_in_transcript_only: false,
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
fn session_agent_transcript_event_is_dropped_after_session_switch() {
    let requested_session_id = lingxi_core::types::SessionId::new();
    let current_session_id = lingxi_core::types::SessionId::new();

    assert!(session_agent_transcript_event(
        requested_session_id,
        current_session_id,
        "agent:test".to_string(),
        Vec::new(),
        0,
    )
    .is_none());

    let event = session_agent_transcript_event(
        requested_session_id,
        requested_session_id,
        "agent:test".to_string(),
        Vec::new(),
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
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "<runtime-reminder>internal</runtime-reminder>".to_string(),
        }],
        is_meta: true,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    let hidden_summary = lingxi_core::types::ConversationMessage::User {
        id: lingxi_core::types::MessageId::new(),
        content: Vec::new(),
        is_meta: false,
        is_compact_summary: true,
        is_visible_in_transcript_only: false,
    };
    let hidden_transcript_only = lingxi_core::types::ConversationMessage::User {
        id: lingxi_core::types::MessageId::new(),
        content: Vec::new(),
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: true,
    };
    let visible = lingxi_core::types::ConversationMessage::Assistant {
        id: lingxi_core::types::MessageId::new(),
        content: Vec::new(),
        stop_reason: None,
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
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "working".to_string(),
                }],
                stop_reason: None,
            },
        })
        .await;
    observer
        .on_event(SubagentObservation::Completed {
            agent_id,
            content: serde_json::json!("done"),
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
                && agent.name == "Design"
                && agent.agent_type == "researcher"
                && agent.status == "running"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentMessage { session_id, agent_id: event_agent_id, message_index, .. }
            if session_id == "session-a"
                && event_agent_id == &agent_id.to_string()
                && *message_index == 0
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::SessionAgentUpdated { session_id, agent }
            if session_id == "session-a"
                && agent.agent_id == agent_id.to_string()
                && agent.name == "Design"
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
                id: lingxi_core::types::MessageId::new(),
                subtype: Some("agent_idle".to_string()),
                content: "idle".to_string(),
                compact_metadata: None,
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
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "follow-up".to_string(),
                }],
                is_meta: true,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
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
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "resumed".to_string(),
                }],
                stop_reason: None,
            },
        })
        .await;
    let events = listener.received.lock().await;
    assert_eq!(events.len(), 4);
    assert!(matches!(
        &events[2],
        ClientEvent::SessionAgentMessage {
            message_index: 3,
            ..
        }
    ));
    assert!(matches!(
        &events[3],
        ClientEvent::SessionAgentUpdated { agent, .. } if agent.status == "running"
    ));
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
    assert_eq!(summary.name, "researcher");
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
    // provider before the orchestrator is wrapped, and the provider's live
    // registry must contain every compiled-in mobile skill.
    assert!(
        rt.orchestrator.has_skill_listing(),
        "mobile orchestrator must expose a skill-listing provider"
    );
    let listed =
        mobile_skill_listing_provider(rt.slash_registry.clone(), rt.session_mode, false, None)
            .skill_entries()
            .await;
    let listed_names: std::collections::BTreeSet<_> =
        listed.iter().map(|entry| entry.name.as_str()).collect();
    let expected_names = ["create-local-app", "local-app-use", "expose-as-mcp"]
        .into_iter()
        .map(|name| format!("{}:{name}", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME))
        .collect::<Vec<_>>();
    assert_eq!(
        expected_names.len(),
        3,
        "a global Code listing must expose exactly the three Local App routers"
    );
    for name in &expected_names {
        assert!(
            listed_names.contains(name.as_str()),
            "mobile skill-listing provider must expose bundled skill {name:?}: {listed_names:?}"
        );
    }
    let listed_local_app_names = listed_names
        .iter()
        .filter(|name| name.starts_with("lingxi-local-app:"))
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let expected_local_app_names = expected_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        listed_local_app_names, expected_local_app_names,
        "a global Code cold boot must hide every app-only Local App skill"
    );
    let local_app_tool_names = rt
        .orchestrator
        .tool_names()
        .into_iter()
        .filter(|name| name.starts_with("LocalApp"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        local_app_tool_names,
        [
            "LocalAppCreate",
            "LocalAppGet",
            "LocalAppList",
            "LocalAppPrepare"
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        "a global Code cold boot must register only the global Local App management \
             tools: the four in `local_apps_tools::GLOBAL_LOCAL_APP_TOOLS`"
    );
    let registry = rt.slash_registry.read().await;
    for name in &expected_names {
        assert!(
            registry.resolve(name).is_some(),
            "compiled-in mobile skill {name:?} must be present in the live slash registry"
        );
    }
    let app_scoped =
        mobile_skill_listing_provider(rt.slash_registry.clone(), rt.session_mode, true, None)
            .skill_entries()
            .await;
    let app_scoped_names: std::collections::BTreeSet<_> =
        app_scoped.iter().map(|entry| entry.name.as_str()).collect();
    for name in crate::mobile::mobile_plugin_skill_names()
        .into_iter()
        .map(|name| format!("{}:{name}", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME))
    {
        assert!(
            app_scoped_names.contains(name.as_str()),
            "an app-scoped listing must expose the full Local App skill set: {name:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn mobile_local_app_scope_id_uses_the_canonical_target_not_the_alias_spelling() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("app data root");
    let app_two = root.path().join("apps/app-two/workspace");
    std::fs::create_dir_all(&app_two).expect("canonical app workspace");
    let alias_parent = root.path().join("apps/app-one");
    std::fs::create_dir_all(&alias_parent).expect("alias parent");
    let alias = alias_parent.join("workspace");
    symlink(&app_two, &alias).expect("A-to-B workspace alias");

    assert_eq!(
        super::mobile_local_app_scope_id(&app_two, root.path()).as_deref(),
        Some("app-two")
    );
    assert_eq!(
        super::mobile_local_app_scope_id(&alias, root.path()).as_deref(),
        Some("app-two"),
        "the Host must carry the canonical target id, never the alias's app-one spelling"
    );

    let outside = tempfile::tempdir().expect("ordinary project");
    let forged = outside.path().join("apps/app-two/workspace");
    std::fs::create_dir_all(&forged).expect("forged suffix");
    assert_eq!(
        super::mobile_local_app_scope_id(&forged, root.path()),
        None,
        "an app-shaped suffix outside the Host data root must stay global"
    );
}

#[tokio::test]
async fn mobile_local_app_exposure_is_host_cwd_bounded_and_chat_stays_isolated() {
    async fn build_for_cwd(
        data_root: &std::path::Path,
        cwd: std::path::PathBuf,
        session_mode: session::jsonl::SessionMode,
    ) -> MobileRuntime {
        std::fs::create_dir_all(&cwd).expect("session cwd");
        let mut cfg = test_config(data_root);
        cfg.cwd = cwd;
        cfg.session_mode = session_mode;
        let platform: Arc<dyn lingxi_core::host::Platform> =
            Arc::new(HostFakePlatform::new(data_root.to_path_buf()));
        let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
        let permission_sink: Arc<dyn PermissionRequestSink> =
            Arc::new(RecordingPermissionSink::default());
        build_mobile(cfg, platform, listener, permission_sink)
            .await
            .expect("build scoped mobile runtime")
    }

    fn local_app_tool_names(runtime: &MobileRuntime) -> std::collections::BTreeSet<String> {
        runtime
            .orchestrator
            .tool_names()
            .into_iter()
            .filter(|name| name.starts_with("LocalApp"))
            .collect()
    }

    async fn local_app_skill_names(runtime: &MobileRuntime) -> std::collections::BTreeSet<String> {
        runtime
            .wired_skill_listing_provider
            .skill_entries()
            .await
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| name.starts_with("lingxi-local-app:"))
            .collect()
    }

    let global_tools = [
        "LocalAppCreate",
        "LocalAppGet",
        "LocalAppList",
        "LocalAppPrepare",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<std::collections::BTreeSet<_>>();
    let global_skills = ["create-local-app", "expose-as-mcp", "local-app-use"]
        .into_iter()
        .map(|name| format!("{}:{name}", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME))
        .collect::<std::collections::BTreeSet<_>>();

    // A project is allowed to contain this directory shape. The Host must
    // not infer Local App authority from the suffix alone.
    let project_root = tempfile::tempdir().expect("project root");
    let forged_project_cwd = project_root
        .path()
        .join("project")
        .join("apps")
        .join("not-a-local-app")
        .join("workspace");
    let forged = build_for_cwd(
        project_root.path(),
        forged_project_cwd,
        session::jsonl::SessionMode::Code,
    )
    .await;
    assert_eq!(
        local_app_tool_names(&forged),
        global_tools,
        "a project with an app-shaped suffix must retain the global three-tool surface"
    );
    assert_eq!(
        local_app_skill_names(&forged).await,
        global_skills,
        "a project with an app-shaped suffix must retain the global three-skill surface"
    );

    let app_root = tempfile::tempdir().expect("app root");
    let app_cwd = app_root
        .path()
        .join("apps")
        .join("app-one")
        .join("workspace");
    let app = build_for_cwd(
        app_root.path(),
        app_cwd.clone(),
        session::jsonl::SessionMode::Code,
    )
    .await;
    let expected_app_tools = crate::mobile::local_apps_tools::LOCAL_APP_TOOLS
        .iter()
        .map(|(name, _, _)| (*name).to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        local_app_tool_names(&app),
        expected_app_tools,
        "a Host-owned Local App workspace must receive the complete builtin surface"
    );
    let expected_app_skills = crate::mobile::mobile_plugin_skill_names()
        .into_iter()
        .map(|name| format!("{}:{name}", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        local_app_skill_names(&app).await,
        expected_app_skills,
        "a Host-owned Local App workspace must receive the complete skill listing"
    );

    let chat = build_for_cwd(app_root.path(), app_cwd, session::jsonl::SessionMode::Chat).await;
    assert!(
        local_app_tool_names(&chat).is_empty(),
        "Chat's existing tool allowlist must remain authoritative in an app workspace"
    );
    assert!(
        local_app_skill_names(&chat).await.is_empty(),
        "Chat must not gain Code-only Local App skills from app cwd classification"
    );
}

#[tokio::test]
async fn mobile_local_app_listing_keeps_chat_visibility_and_other_plugins() {
    let registry = Arc::new(tokio::sync::RwLock::new(command_api::CommandRegistry::new()));
    let mut chat_frontmatter = command_api::CommandFrontmatter::default();
    chat_frontmatter.session_modes = Some(vec!["chat".to_string()]);
    for name in [
        "third-party:chat-router",
        "lingxi-local-app:create-local-app",
    ] {
        let frontmatter = if name.starts_with("lingxi-local-app:") {
            command_api::CommandFrontmatter::default()
        } else {
            chat_frontmatter.clone()
        };
        registry
            .write()
            .await
            .register_command(command_api::SlashCommand {
                name: name.to_string(),
                description: "router".to_string(),
                source: command_api::CommandSource::Plugin,
                kind: command_api::SlashCommandKind::Markdown {
                    file_path: std::path::PathBuf::from("/virtual/SKILL.md"),
                    frontmatter,
                    prompt_template: "route".to_string(),
                },
                loaded_from: Some("plugin".to_string()),
                has_user_specified_description: true,
                ..command_api::SlashCommand::default()
            });
    }

    let chat = mobile_skill_listing_provider(
        registry.clone(),
        session::jsonl::SessionMode::Chat,
        false,
        None,
    )
    .skill_entries()
    .await;
    assert!(
        chat.iter()
            .any(|entry| entry.name == "third-party:chat-router"),
        "Chat's existing session-mode visibility must remain intact"
    );
    assert!(
        !chat
            .iter()
            .any(|entry| entry.name == "lingxi-local-app:create-local-app"),
        "the Local App router must not bypass Chat's existing frontmatter gate"
    );

    let code =
        mobile_skill_listing_provider(registry, session::jsonl::SessionMode::Code, false, None)
            .skill_entries()
            .await;
    assert!(code
        .iter()
        .any(|entry| entry.name == "third-party:chat-router"));
    assert!(code
        .iter()
        .any(|entry| entry.name == "lingxi-local-app:create-local-app"));
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
    write_skill(
        tmp.path(),
        "frontend-design",
        "Decoy frontend-design",
        "DECOY FRONTEND DESIGN",
    );
    write_skill(
        tmp.path(),
        "lingxi-local-app:frontend-design",
        "Reserved namespace decoy",
        "RESERVED NAMESPACE DECOY",
    );
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

    let listed = provider.skill_entries().await;
    let listed_names: std::collections::BTreeSet<_> =
        listed.iter().map(|entry| entry.name.as_str()).collect();
    let mut expected = vec!["loop".to_string(), "foo".to_string()];
    expected.extend(
        ["create-local-app", "local-app-use", "expose-as-mcp"]
            .into_iter()
            .map(|name| format!("{}:{name}", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME)),
    );
    assert_eq!(
        expected.len(),
        5,
        "loop + foo + three global Local App routers must be listed"
    );
    for name in &expected {
        assert!(
            listed_names.contains(name.as_str()),
            "live mobile listing must contain {name:?}: {listed_names:?}"
        );
        let desc = loader
            .load(name)
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
        .load("foo")
        .await
        .expect("load ok")
        .expect("foo present");
    assert!(foo_v1.body.contains("FOO BODY v1"));
    let frontend_design_name = format!(
        "{}:frontend-design",
        crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
    );
    let frontend_design = loader
        .load(&frontend_design_name)
        .await
        .expect("load ok")
        .expect("frontend-design present");
    assert!(
        !frontend_design.body.contains("DECOY FRONTEND DESIGN"),
        "same-name disk decoy must not override the file-backed Plugin frontend-design"
    );
    let loop_desc = loader
        .load("loop")
        .await
        .expect("load ok")
        .expect("loop present");
    let loop_prompt = loop_desc
        .dynamic_body
        .as_ref()
        .expect("bundled loop stays programmatic")
        .build("");
    assert!(
        !loop_prompt.contains("DECOY LOOP BODY"),
        "same-name disk decoy must not override bundled loop"
    );
    let registry = rt.slash_registry.read().await;
    let resolved_loop = registry.resolve("loop").expect("loop resolves");
    assert_eq!(
        resolved_loop.loaded_from.as_deref(),
        Some("bundled"),
        "loop must resolve from bundled after boot restore"
    );
    drop(registry);
    let listed_loop = listed
        .iter()
        .find(|entry| entry.name == "loop")
        .expect("loop listed");
    assert!(listed_loop.is_bundled, "loop must list as bundled");

    write_skill(tmp.path(), "foo", "Foo skill", "FOO BODY v2");
    match rt.dispatcher.dispatch("/reload-skills").await {
        SlashDispatchResult::Handled { .. } => {}
        other => panic!("reload-skills must be handled locally, got {other:?}"),
    }
    let foo_v2 = loader
        .load("foo")
        .await
        .expect("load ok")
        .expect("foo present after reload");
    assert!(foo_v2.body.contains("FOO BODY v2"));
    let loop_after_reload = loader
        .load("loop")
        .await
        .expect("load ok")
        .expect("loop still present after reload");
    let loop_prompt_after_reload = loop_after_reload
        .dynamic_body
        .as_ref()
        .expect("bundled loop stays programmatic")
        .build("");
    assert!(
        !loop_prompt_after_reload.contains("DECOY LOOP BODY"),
        "bundled loop must survive reload precedence"
    );

    std::fs::remove_dir_all(tmp.path().join(".lingxi").join("skills").join("foo"))
        .expect("remove foo skill");
    match rt.dispatcher.dispatch("/reload-skills").await {
        SlashDispatchResult::Handled { .. } => {}
        other => panic!("reload-skills must be handled locally, got {other:?}"),
    }
    let listed_after_delete = provider.skill_entries().await;
    let listed_after_delete_names: std::collections::BTreeSet<_> = listed_after_delete
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(
        !listed_after_delete_names.contains("foo"),
        "deleted disk skill must disappear from live listing: {listed_after_delete_names:?}"
    );
    assert!(
        loader.load("foo").await.expect("load ok").is_none(),
        "deleted disk skill must disappear from the shared Skill loader"
    );
    let frontend_design_after_delete = loader
        .load(&frontend_design_name)
        .await
        .expect("load ok")
        .expect("frontend-design still present");
    assert!(
        !frontend_design_after_delete
            .body
            .contains("DECOY FRONTEND DESIGN"),
        "the file-backed Plugin frontend-design must survive repeated reloads"
    );
    assert!(!frontend_design_after_delete
        .body
        .contains("RESERVED NAMESPACE DECOY"));
    let loop_after_delete = loader
        .load("loop")
        .await
        .expect("load ok")
        .expect("loop still present after delete reload");
    let loop_prompt_after_delete = loop_after_delete
        .dynamic_body
        .as_ref()
        .expect("bundled loop stays programmatic")
        .build("");
    assert!(
        !loop_prompt_after_delete.contains("DECOY LOOP BODY"),
        "bundled loop precedence must survive repeated reloads"
    );
    let registry_after_delete = rt.slash_registry.read().await;
    let resolved_loop_after_delete = registry_after_delete
        .resolve("loop")
        .expect("loop resolves after reload");
    assert_eq!(
        resolved_loop_after_delete.loaded_from.as_deref(),
        Some("bundled"),
        "loop must still resolve from bundled after repeated reloads"
    );
    drop(registry_after_delete);

    let plugin_id = crate::mobile::mobile_builtin_plugin_id();
    rt.wired_plugin_manager
        .disable(&plugin_id)
        .await
        .expect("disable builtin Plugin");
    match rt.dispatcher.dispatch("/reload-skills").await {
        SlashDispatchResult::Handled { .. } => {}
        other => panic!("reload-skills after disable must be handled, got {other:?}"),
    }
    assert!(
        loader
            .load(&frontend_design_name)
            .await
            .expect("load after disable/reload")
            .is_none(),
        "a disk decoy cannot resurrect a disabled Plugin namespace"
    );
    let (manifest, install_dir) = match rt.wired_plugin_manager.plugin_state(&plugin_id).await {
        Some(plugin::PluginState::Disabled {
            manifest,
            install_dir,
        }) => (manifest, install_dir),
        other => panic!("expected disabled builtin Plugin, got {other:?}"),
    };
    rt.wired_plugin_manager
        .enable(&plugin_id, manifest, install_dir)
        .await
        .expect("re-enable builtin Plugin");
    let restored = loader
        .load(&frontend_design_name)
        .await
        .expect("load after re-enable")
        .expect("Plugin skill restored");
    assert!(!restored.body.contains("RESERVED NAMESPACE DECOY"));
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
    let listed = rt.wired_skill_listing_provider.skill_entries().await;
    assert!(
        listed.iter().any(|entry| entry.name == namespaced_cmd),
        "the plugin's command must appear in the live listing after \
             PluginManager::enable: {:?}",
        listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
    );

    // Invocation: the Skill tool's OWN loader — `wired_skill_loader` —
    // must resolve the identical name to the identical body. If
    // `PluginManager` had been handed a `CommandRegistry` of its own
    // instead of `shared_command_registry`, the listing check above and
    // this one could both still fail (or, worse, only one of them would),
    // which is exactly the split §19.2 forbids.
    let loaded = rt
        .wired_skill_loader
        .load(&namespaced_cmd)
        .await
        .expect("load ok")
        .unwrap_or_else(|| panic!("plugin command {namespaced_cmd:?} must be invocable"));
    assert!(
        loaded.body.contains("FIXTURE COMMAND BODY"),
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

/// P1.10 (§19.2) — the required gate: after booting through the REAL
/// production path (`build_mobile` → `build_mobile_inner`, never a
/// hand-built `PluginManager` or a direct call to
/// `builtin_bundle`/`local_apps::pack`), the ONE compiled-in mobile
/// plugin's live components must come from a materialized,
/// digest-verified on-disk root — not from a manifest registered over
/// `PathBuf::new()` with `PluginComponents::default()`, the pre-P1.10
/// state in which registration trivially "succeeded" having loaded
/// nothing.
///
/// Every assertion below is deliberately non-vacuous: a component COUNT
/// derived from the packer's OWN resolved inventory (never a hardcoded
/// literal that could rot independently of the embedded file set), and a
/// NAMED skill's body content asserted equal to what a completely
/// separate call to the SAME materializer function finds on disk at the
/// verified root. Two zero counts, or two empty bodies, would both
/// satisfy a weaker version of this test — see the module docs on why
/// that shape is exactly the defect class this task exists to close.
#[tokio::test]
async fn mobile_boot_materializes_the_builtin_bundle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
    let perm_sink: Arc<dyn PermissionRequestSink> = Arc::new(RecordingPermissionSink::default());
    let rt = build_mobile(test_config(tmp.path()), platform, listener, perm_sink)
        .await
        .expect("build_mobile failed");

    // --- Independent, non-production re-derivation of what the boot
    // path SHOULD have materialized: the exact same bundle_root
    // `build_mobile_inner` computes (`test_config`'s `lingxi_home` is
    // `<cwd>/.claude`), fed to the very same materializer function. This
    // is not a second implementation to compare against — it is the
    // identical function, called a second time, which the §6.2
    // short-circuit makes a cheap no-op if boot already promoted this
    // digest. It exists so the test can name a concrete on-disk path and
    // read real file content from it, without reaching into
    // `PluginManager`'s private state.
    let bundle_root = tmp
        .path()
        .join(branding::DOT_DIR)
        .join("builtin-plugin-bundle");
    let (verified_root, resolved_inventory) =
        crate::mobile::builtin_bundle::materialize_compiled_in_plugin_bundle(&bundle_root, None)
            .expect("the compiled-in bundle must already be materialized by boot");
    assert!(
        verified_root.is_dir(),
        "the verified root boot promoted must exist on disk: {}",
        verified_root.display()
    );
    let root_dir_name = verified_root
        .file_name()
        .and_then(|n| n.to_str())
        .expect("verified root must have a UTF-8 directory name");
    let digest_suffix = root_dir_name
        .strip_prefix("root-")
        .expect("the verified root must be digest-named (root-<sha256>)");
    assert_eq!(
        digest_suffix.len(),
        64,
        "the digest suffix must be a full SHA-256 hex string, got {digest_suffix:?} \
             — a shorter/synthetic name would mean this was never actually digest-verified"
    );

    // Real content, read straight off the verified root — not a fixture
    // string this test invented.
    let device_skill_on_disk =
        std::fs::read_to_string(verified_root.join("skills/device/SKILL.md"))
            .expect("skills/device/SKILL.md must exist at the verified root");
    let real_skill_marker = ["window", ".", "lingxi", ".v2"].concat();
    assert!(
        device_skill_on_disk.contains(&real_skill_marker),
        "the verified root's own file content must be the real plugin skill, got: \
             {device_skill_on_disk}"
    );

    // --- The production-visible half: the plugin `PluginManager` loaded
    // through `build_mobile_inner` must have actually READ that root, not
    // merely have a manifest that happens to name it.
    let plugin_id = crate::mobile::mobile_builtin_plugin_id();
    assert!(
        rt.wired_plugin_manager
            .loaded_plugin_ids()
            .await
            .contains(&plugin_id),
        "the compiled-in plugin must be Loaded after boot"
    );
    // TypeScript is host-global: the Local App plugin materialization above
    // must not own this descriptor or unload it when the plugin is toggled.
    let lsp_name = crate::mobile::mobile_lsp::GLOBAL_TYPESCRIPT_LSP_SERVER_NAME;
    let lsp_config = rt
        .lsp_registry
        .get_config(lsp_name)
        .await
        .unwrap_or_else(|| {
            panic!("{lsp_name} must register in the shared global mobile LSP registry")
        });
    assert_eq!(
        lsp_config.command,
        "/opt/lingxi/toolchains/typescript/7.0.2/tsc"
    );

    // Agents: count derived from the packer's OWN resolved inventory, so
    // this can never rot into a hardcoded literal independent of
    // `EMBEDDED_PLUGIN_FILES`. Nonzero and specific — not the "0 == 0"
    // shape a manifest-over-nothing would also satisfy.
    let expected_agent_count = resolved_inventory
        .iter()
        .filter(|entry| entry.path.starts_with("agents/") && entry.path.ends_with(".md"))
        .count();
    assert!(
        expected_agent_count > 0,
        "fixture bug: the compiled-in plugin must embed at least one agent"
    );
    let plugin_agent_names: Vec<String> = rt
        .wired_agent_catalog
        .read()
        .await
        .iter()
        .map(|def| def.agent_type.clone())
        .filter(|name| name.starts_with(&format!("{}:", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME)))
        .collect();
    assert_eq!(
        plugin_agent_names.len(),
        expected_agent_count,
        "the live agent catalog must carry exactly the packer-resolved agent count \
             for this plugin, got {plugin_agent_names:?}"
    );
    assert!(
        plugin_agent_names.contains(&format!(
            "{}:mcp-designer",
            crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
        )),
        "the real `mcp-designer` agent must be present, named, in the live catalog: \
             {plugin_agent_names:?}"
    );
    // The create path no longer runs a build workflow, so that workflow's
    // role agents went with it. Pin their ABSENCE: an agent file left
    // behind re-advertises a stage the flow no longer has, and the count
    // assertion above would not notice a swap of one agent for another.
    for retired in [
        "builder",
        "designer",
        "template-selector",
        "create-preparer",
    ] {
        assert!(
            !plugin_agent_names.contains(&format!(
                "{}:{retired}",
                crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
            )),
            "the retired create agent `{retired}` must not be in the live catalog: \
                 {plugin_agent_names:?}"
        );
    }

    // Skills: the plugin's skills are registered as namespaced slash
    // commands (`plugin::manager::load_plugin`'s skill arm) into the SAME
    // shared command registry the model's per-turn listing and the Skill
    // tool's loader both read — `wired_skill_listing_provider` /
    // `wired_skill_loader` below, exactly as
    // `listing_and_invocation_share_one_registry` above exercises for a
    // fixture plugin.
    let namespaced_device_skill = format!("{}:device", crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME);
    let listed = rt.wired_skill_listing_provider.skill_entries().await;
    assert!(
        !listed
            .iter()
            .any(|entry| entry.name == namespaced_device_skill),
        "a global session must not list app-only Local App skills: {:?}",
        listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
    );
    let loaded_device_skill = rt
        .wired_skill_loader
        .load(&namespaced_device_skill)
        .await
        .expect("load ok")
        .unwrap_or_else(|| panic!("{namespaced_device_skill} must be invocable"));
    // The decisive tie: the LIVE, invocable body must be the SAME real
    // content this test read directly off the verified root above — not
    // merely both non-empty, but the actual materialized bytes.
    assert!(
        loaded_device_skill.body.contains(&real_skill_marker),
        "the live skill body must be the verified root's own real content, got: {:?}",
        loaded_device_skill.body
    );

    // The real subagent spawner must preload the bare skills declared by
    // Plugin agents through that same registry. The plugin prefix is
    // derived from `lingxi-local-app:verifier`, exactly like desktop.
    let agent_skill_loader = rt
        .wired_subagent_skill_loader_cell
        .get()
        .expect("mobile subagent skill-preload cell must be filled");
    let preloaded = agent_skill_loader
        .resolve_and_load("frontend-qa", "lingxi-local-app:verifier", None)
        .await
        .expect("checked skill preload")
        .expect("Plugin agent bare skill must resolve through its namespace");
    assert!(matches!(
        preloaded.content.as_slice(),
        [lingxi_core::types::ContentBlock::Text { text }]
            if text.contains("# Frontend QA")
                && text.contains("Verify the running app, not only the build output.")
    ));

    let frontend_root = loaded_device_skill
        .skill_root
        .as_ref()
        .and_then(|device_root| device_root.parent())
        .map(|skills_root| skills_root.join("frontend-design"))
        .expect("materialized Plugin skill root");
    assert!(
        frontend_root.join("references/router.md").is_file(),
        "file-backed migrated references must remain reachable from the materialized skill root"
    );
    assert!(rt
        .wired_skill_loader
        .load("lingxi-local-app:openai")
        .await
        .expect("load ok")
        .is_none());

    // Workflows: the manager, model-facing tool, launcher, and nested task
    // handler must share the exact same registry allocation. The loaded
    // snapshot must be the real verified-root bytes under its namespaced
    // meta.name, not a hand-seeded test entry.
    let workflow_registry = &rt.wired_plugin_workflow_registry;
    assert!(rt
        .wired_plugin_manager
        .shares_plugin_workflows(workflow_registry));
    assert!(rt
        .wired_workflow_tool
        .shares_plugin_workflows(workflow_registry));
    assert!(rt
        .wired_local_workflow_handler
        .shares_plugin_workflows(workflow_registry));
    assert!(Arc::ptr_eq(
        &rt.workflow_launcher.plugin_workflows,
        workflow_registry
    ));
    let namespaced_workflow = format!(
        "{}:local-app-use-test",
        crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
    );
    let resolved = workflow_registry
        .resolve(&namespaced_workflow)
        .unwrap_or_else(|| panic!("{namespaced_workflow} must resolve after production boot"));
    let workflow_on_disk =
        std::fs::read_to_string(verified_root.join("workflows/local-app-use-test.js"))
            .expect("materialized workflow");
    assert_eq!(
        std::fs::read_to_string(&resolved).expect("resolved workflow"),
        workflow_on_disk
    );
    assert_eq!(
        resolved,
        std::fs::canonicalize(verified_root.join("workflows/local-app-use-test.js"))
            .expect("canonical materialized workflow")
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
            if reason.starts_with("Permission to use ExitPlanMode has been denied.")
    ));

    // Drive a `check()` on a spawned task; a deny-by-default tool parks a
    // request on the sink (proving the adapter gate is bound, not a no-op).
    let gate = rt.permission_gate.clone();
    let g = gate.clone();
    let task = tokio::spawn(async move {
        use permission::gate::PermissionGate;
        g.check("Bash", &serde_json::json!({"command": "ls"})).await
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
        app_data_root: tmp.path().to_path_buf(),
        current_cwd: Arc::new(std::sync::Mutex::new(tmp.path().to_path_buf())),
        lingxi_home: tmp.path().join(".claude"),
        // The launcher and status sink must share the engine's live
        // session watermark; a detached fixture uuid would correctly
        // suppress the completion event as stale.
        session_uuid: rt.active_session_uuid.clone(),
        default_model_selection_provider: Arc::new(std::sync::OnceLock::new()),
        checkpoints: rt.workflow_checkpoints.clone(),
        status_sink: rt.workflow_status_sink.clone(),
        plugin_workflows: rt.wired_plugin_workflow_registry.clone(),
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
    let notes = rt.task_registry.take_pending_task_notifications().await;
    assert!(
        notes.iter().any(|n| n.task_id == launched.task_id),
        "completed workflow must be drained as a task notification: {notes:?}"
    );
    let again = rt.task_registry.take_pending_task_notifications().await;
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
        app_data_root: tmp.path().to_path_buf(),
        current_cwd,
        lingxi_home: tmp.path().join(".claude"),
        session_uuid: rt.active_session_uuid.clone(),
        default_model_selection_provider: Arc::new(std::sync::OnceLock::new()),
        checkpoints: rt.workflow_checkpoints.clone(),
        status_sink: rt.workflow_status_sink.clone(),
        plugin_workflows: rt.wired_plugin_workflow_registry.clone(),
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

    // R-P1: claudeMd lives in the leading additional-context `<system-reminder>`
    // meta now (same `memory_block::format`), NOT the system prompt.
    let ctx = rt
        .orchestrator
        .additional_context_preview()
        .await
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

    let sys = rt.orchestrator.assemble_system_prompt_preview().await;
    assert!(
            !sys.contains(
                "Codebase and user instructions are shown below. Be sure to adhere to these instructions."
            ),
            "a default build must emit NO memory preamble: {sys}"
        );
}

// ── F3-05: the async `submit` FFI entry point ───────────────────────────

use super::{build_mobile_engine, MobileEngineHandle};
use crate::mobile::local_apps_llm::test_support::ScriptedModel;
use client::protocol::commands::ClientCommand;
use client::protocol::error::ClientError;
use client::protocol::events::{ClientEvent as Ev, TurnRecoveryStateDto};
use client::protocol::permission::PermissionResponseDto;
use lingxi_core::host::audio::{
    AudioCapabilitySnapshot, AudioError, AudioOperation, AudioOperationContext, AudioOperationId,
    AudioOperationKind, AudioOperationSuccess, AudioOwner, AudioRecordingHandle, AudioService,
};

/// Build a real, fully-wired [`MobileEngineHandle`] off-device (host fake
/// `Platform`) so the F3-05 `submit` path is exercised on CI. Returns the
/// handle plus the recording listener so a test can read back delivered
/// events.
///
/// Task 11: `CreateApp` (and friends) now trigger a REAL background
/// authoring/planning round trip. Off-device tests have no network, so
/// this installs a deterministic, always-fails-fast local-apps model
/// (zero scripted responses ⇒ an immediate, in-process
/// `AppError::Io`, no I/O) instead of leaving every caller race a real
/// `api.anthropic.com` request — a test that wants a scripted success
/// overrides it via `set_local_apps_model` before triggering.
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
    handle.set_local_apps_model(ScriptedModel::new());
    (handle, listener)
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
fn plugin_command_reaches_the_manager_and_reports_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::GetStatus {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                },
            })
            .await
            .expect("GetStatus should route to the live plugin manager");
        let events = listener.received.lock().await.clone();
        assert!(
            events.iter().any(|event| {
                matches!(
                    event,
                    Ev::AppEvent {
                        event: client::protocol::local_apps::AppEventDto::PluginStatusChanged {
                            status,
                        },
                    } if status.plugin_id == crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
                        && matches!(
                            status.state,
                            client::protocol::local_apps::PluginActivationStateDto::Loaded
                        )
                )
            }),
            "GetStatus must emit a Loaded PluginStatusChanged event: {events:?}"
        );
    });
}

#[test]
fn enabled_plugins_three_way_uses_manifest_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let settings = tmp.path().join("settings.json");

    assert!(!super::mobile_builtin_plugin_enabled(&settings, false).unwrap());
    std::fs::write(&settings, r#"{"enabledPlugins":{}}"#).unwrap();
    assert!(!super::mobile_builtin_plugin_enabled(&settings, false).unwrap());
    std::fs::write(&settings, r#"{"enabledPlugins":{"lingxi-local-app":true}}"#).unwrap();
    assert!(super::mobile_builtin_plugin_enabled(&settings, false).unwrap());
    std::fs::write(
        &settings,
        r#"{"enabledPlugins":{"lingxi-local-app":false}}"#,
    )
    .unwrap();
    assert!(!super::mobile_builtin_plugin_enabled(&settings, true).unwrap());

    std::fs::write(&settings, r#"{"enabledPlugins":[]}"#).unwrap();
    assert!(super::mobile_builtin_plugin_enabled(&settings, true).is_err());
    std::fs::write(
        &settings,
        r#"{"enabledPlugins":{"lingxi-local-app":"false"}}"#,
    )
    .unwrap();
    assert!(super::mobile_builtin_plugin_enabled(&settings, true).is_err());
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

#[test]
fn toggling_enabled_emits_plugin_status_changed_and_persists_the_bare_key() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let command = |enabled| ClientCommand::PluginCommand {
            command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                enabled,
            },
        };
        handle
            .submit(command(false))
            .await
            .expect("disable should route to PluginManager::disable");
        let disabled = listener.received.lock().await.clone();
        assert!(
            disabled.iter().any(|event| {
                matches!(
                    event,
                    Ev::AppEvent {
                        event: client::protocol::local_apps::AppEventDto::PluginStatusChanged {
                            status,
                        },
                    } if status.plugin_id == crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
                        && matches!(
                            status.state,
                            client::protocol::local_apps::PluginActivationStateDto::Disabled
                        )
                )
            }),
            "disable must emit Disabled PluginStatusChanged: {disabled:?}"
        );
        assert_eq!(
            handle
                .inner
                .wired_plugin_workflow_registry
                .resolve("lingxi-local-app:local-app-use-test"),
            None,
            "disable must unload plugin workflows from the shared registry"
        );
        let namespaced_skill = "lingxi-local-app:frontend-design";
        assert!(handle
            .inner
            .wired_skill_loader
            .load(namespaced_skill)
            .await
            .expect("load after disable")
            .is_none());
        assert!(!handle
            .inner
            .wired_skill_listing_provider
            .skill_entries()
            .await
            .iter()
            .any(|entry| entry.name == namespaced_skill));
        assert_eq!(
            super::mobile_live_plugin_skill_count(&handle.inner.slash_registry).await,
            0,
            "live FFI count source must agree with disabled listing"
        );
        assert!(
            handle
                .inner
                .lsp_registry
                .get_config(crate::mobile::mobile_lsp::GLOBAL_TYPESCRIPT_LSP_SERVER_NAME)
                .await
                .is_some(),
            "global TypeScript LSP must not follow the Local App plugin toggle"
        );
        let settings =
            std::fs::read_to_string(tmp.path().join(branding::DOT_DIR).join("settings.json"))
                .expect("disable should persist settings.json");
        let settings: serde_json::Value = serde_json::from_str(&settings).expect("settings");
        assert_eq!(
            settings["enabledPlugins"][crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME],
            false
        );

        handle
            .submit(command(true))
            .await
            .expect("enable should materialize and route to PluginManager");
        let enabled = listener.received.lock().await.clone();
        assert!(
            enabled.iter().any(|event| {
                matches!(
                    event,
                    Ev::AppEvent {
                        event: client::protocol::local_apps::AppEventDto::PluginStatusChanged {
                            status,
                        },
                    } if status.plugin_id == crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME
                        && matches!(
                            status.state,
                            client::protocol::local_apps::PluginActivationStateDto::Loaded
                        )
                )
            }),
            "enable must emit Loaded PluginStatusChanged: {enabled:?}"
        );
        assert!(
            handle
                .inner
                .wired_plugin_workflow_registry
                .resolve("lingxi-local-app:local-app-use-test")
                .is_some(),
            "enable must restore plugin workflows in the shared registry"
        );
        assert!(handle
            .inner
            .wired_skill_loader
            .load(namespaced_skill)
            .await
            .expect("load after re-enable")
            .is_some());
        assert!(
            !handle
                .inner
                .wired_skill_listing_provider
                .skill_entries()
                .await
                .iter()
                .any(|entry| entry.name == namespaced_skill),
            "re-enabling must restore the complete live registry without leaking an \
                 app-only skill into this global conversation's listing"
        );
        assert_eq!(
            super::mobile_live_plugin_skill_count(&handle.inner.slash_registry).await,
            crate::mobile::mobile_plugin_skill_names().len(),
            "re-enable must restore the complete live registry even though the \
                 global listing exposes only its three routers"
        );
        let settings =
            std::fs::read_to_string(tmp.path().join(branding::DOT_DIR).join("settings.json"))
                .expect("enable should persist settings.json");
        let settings: serde_json::Value = serde_json::from_str(&settings).expect("settings");
        assert_eq!(
            settings["enabledPlugins"][crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME],
            true
        );
    });
}

#[test]
fn disabled_state_is_present_and_status_survives_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    {
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::PluginCommand {
                    command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                        plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                        enabled: false,
                    },
                })
                .await
                .expect("disable");
            assert!(matches!(
                handle
                    .inner
                    .plugin_manager
                    .plugin_state(&crate::mobile::mobile_builtin_plugin_id())
                    .await,
                Some(plugin::PluginState::Disabled { .. })
            ));
        });
    }

    let (restarted, listener) = build_submit_handle(tmp.path());
    assert_eq!(
        restarted.skill_count(),
        0,
        "an explicitly disabled Plugin must report zero live skills after restart"
    );
    restarted.runtime().block_on(async {
        assert!(
            restarted
                .inner
                .plugin_manager
                .plugin_state(&crate::mobile::mobile_builtin_plugin_id())
                .await
                .is_none(),
            "a disabled boot must not materialize/register the plugin merely \
                 to manufacture a generic PluginManager Disabled state"
        );
        assert!(restarted
            .inner
            .wired_plugin_workflow_registry
            .resolve("lingxi-local-app:local-app-use-test")
            .is_none());
        restarted
            .submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::GetStatus {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                },
            })
            .await
            .expect("status after restart");
        assert!(listener.received.lock().await.iter().any(|event| {
            matches!(
                event,
                Ev::AppEvent {
                    event: client::protocol::local_apps::AppEventDto::PluginStatusChanged {
                        status,
                    },
                } if matches!(
                    status.state,
                    client::protocol::local_apps::PluginActivationStateDto::Disabled
                )
            )
        }));
    });
}

#[test]
fn disabled_boot_defers_bundle_materialization_until_enable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let settings_dir = tmp.path().join(branding::DOT_DIR);
    std::fs::create_dir_all(&settings_dir).expect("settings directory");
    std::fs::write(
        settings_dir.join("settings.json"),
        serde_json::json!({
            "enabledPlugins": {
                "lingxi-local-app": false
            }
        })
        .to_string(),
    )
    .expect("disabled settings");

    // A pre-existing container makes the negative assertion meaningful:
    // merely checking that an absent directory stays absent cannot catch a
    // future sync path that reads/repairs an existing plugin root.
    let bundle_root = settings_dir.join("builtin-plugin-bundle");
    std::fs::create_dir_all(&bundle_root).expect("stale bundle container");
    let sentinel = bundle_root.join("disabled-boot-sentinel");
    std::fs::write(&sentinel, b"must remain the only entry").expect("bundle sentinel");
    let bundle_entries = || {
        std::fs::read_dir(&bundle_root)
            .expect("bundle entries")
            .map(|entry| {
                entry
                    .expect("bundle entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<std::collections::BTreeSet<_>>()
    };

    let (handle, listener) = build_submit_handle(tmp.path());
    assert_eq!(
        bundle_entries(),
        ["disabled-boot-sentinel".to_string()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        "disabled boot must not materialize or repair an existing compiled-plugin container"
    );
    handle.runtime().block_on(async {
        assert!(
            handle
                .inner
                .plugin_manager
                .plugin_state(&crate::mobile::mobile_builtin_plugin_id())
                .await
                .is_none(),
            "disabled boot should retain no materialized PluginManager state"
        );
        for command in [
            client::protocol::local_apps::PluginCommandDto::GetStatus {
                plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
            },
            client::protocol::local_apps::PluginCommandDto::GetInventory {
                plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
            },
        ] {
            handle
                .submit(ClientCommand::PluginCommand { command })
                .await
                .expect("disabled metadata query");
        }
    });
    assert_eq!(
        bundle_entries(),
        ["disabled-boot-sentinel".to_string()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        "compiled status/inventory queries must not enter plugin filesystem materialization"
    );
    assert_eq!(
        std::fs::read(&sentinel).expect("sentinel after metadata queries"),
        b"must remain the only entry"
    );
    let events = listener.received.blocking_lock().clone();
    assert!(events.iter().any(|event| matches!(
        event,
        Ev::AppEvent {
            event: client::protocol::local_apps::AppEventDto::PluginStatusChanged { status }
        } if matches!(
            status.state,
            client::protocol::local_apps::PluginActivationStateDto::Disabled
        )
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Ev::AppEvent {
            event: client::protocol::local_apps::AppEventDto::PluginInventoryChanged {
                inventory
            }
        } if matches!(
            inventory.state,
            client::protocol::local_apps::PluginActivationStateDto::Disabled
        ) && inventory.counts.skills > 0
    )));

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                    enabled: true,
                },
            })
            .await
            .expect("enable should use the existing registration path");
    });
    assert!(
        bundle_root
            .join("materialized")
            .join("active.manifest.json")
            .is_file(),
        "enabling must be the first operation that materializes and activates the bundle"
    );
}

#[test]
fn concurrent_plugin_toggles_leave_disk_and_registry_consistent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        for _ in 0..4 {
            let disable = handle.submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                    enabled: false,
                },
            });
            let enable = handle.submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                    enabled: true,
                },
            });
            let (disable, enable) = tokio::join!(disable, enable);
            disable.expect("concurrent disable");
            enable.expect("concurrent enable");

            let is_loaded = matches!(
                handle
                    .inner
                    .plugin_manager
                    .plugin_state(&crate::mobile::mobile_builtin_plugin_id())
                    .await,
                Some(plugin::PluginState::Loaded { .. })
            );
            let settings =
                std::fs::read_to_string(tmp.path().join(branding::DOT_DIR).join("settings.json"))
                    .expect("settings after concurrent toggles");
            let settings: serde_json::Value =
                serde_json::from_str(&settings).expect("valid settings JSON");
            assert_eq!(
                settings["enabledPlugins"][crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME].as_bool(),
                Some(is_loaded),
                "atomic settings transaction must agree with the live registry"
            );
        }
    });
}

#[test]
fn failed_plugin_setting_write_rolls_live_state_back() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _) = build_submit_handle(tmp.path());
    let settings = tmp.path().join(branding::DOT_DIR).join("settings.json");
    let _ = std::fs::remove_file(&settings);
    std::fs::create_dir_all(&settings).expect("replace settings file with directory");

    handle.runtime().block_on(async {
        let error = handle
            .submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                    enabled: false,
                },
            })
            .await
            .expect_err("settings write must fail");
        assert!(error
            .to_string()
            .contains("persist mobile plugin setting failed"));
        assert!(matches!(
            handle
                .inner
                .plugin_manager
                .plugin_state(&crate::mobile::mobile_builtin_plugin_id())
                .await,
            Some(plugin::PluginState::Loaded { .. })
        ));
        assert!(handle
            .inner
            .wired_plugin_workflow_registry
            .resolve("lingxi-local-app:local-app-use-test")
            .is_some());
    });
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
    handle.set_local_apps_model(ScriptedModel::new());
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
    handle.set_local_apps_model(ScriptedModel::new());
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
        _input: serde_json::Value,
    ) -> Result<lingxi_core::host::McpToolResultDto, lingxi_core::host::McpError> {
        Err(lingxi_core::host::McpError::Internal(
            "unused test tool call".into(),
        ))
    }

    async fn read_resource(
        &self,
        _conn: &lingxi_core::host::McpRawConnection,
        _uri: &str,
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
            .begin(&session_id, 77, "hello".to_string(), None, Vec::new())
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
            .begin(&session_id, 78, "hello".to_string(), None, Vec::new())
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
    let tmp = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(McpRegistry::new(Arc::new(LocalAppsMcpTransport::new(
        tmp.path().join("apps"),
    ))));
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
    let tmp = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(McpRegistry::new(Arc::new(LocalAppsMcpTransport::new(
        tmp.path().join("apps"),
    ))));
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
                .begin(&session_id, turn_id, "hello".to_string(), None, Vec::new())
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
                state,
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
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let session_id = handle.inner.orchestrator.current_session_id().await;
        let dir = orchestrator::transcript_paths::subagents_dir(
            &handle.lingxi_home,
            &handle.session_cwd,
            &session_id.as_uuid().to_string(),
        )
        .join("workflows")
        .join("wf_nested");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create nested workflow dir");
        let expected_agent_id = lingxi_core::types::AgentId::new().to_string();
        let transcript_path = dir.join(format!("agent-{expected_agent_id}.jsonl"));
        let body = [
            serde_json::to_string(&serde_json::json!({
                "agent_name": "wf child",
                "agent_type": "workflow-subagent",
                "status": "running"
            }))
            .unwrap(),
            serde_json::to_string(&serde_json::json!({
                "message": lingxi_core::types::ConversationMessage::Assistant {
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![lingxi_core::types::ContentBlock::Text {
                        text: "nested workflow child".to_string(),
                    }],
                    stop_reason: None,
                }
            }))
            .unwrap(),
        ]
        .join("\n")
            + "\n";
        tokio::fs::write(&transcript_path, body)
            .await
            .expect("write nested transcript");

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
        assert!(loaded.iter().any(|event| matches!(
            event,
            Ev::SessionAgentTranscript { agent_id, messages, .. }
                if agent_id == &expected_agent_id
                    && messages.iter().any(|message| message.blocks.iter().any(|block| {
                        matches!(
                            block,
                            client::protocol::message::MessageBlockDto::Text { text }
                                if text.contains("nested workflow child")
                        )
                    }))
        )));
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
            _: String,
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
            assert_eq!(registry.take_human_task_messages_for(agent_id).await, vec!["  continue with care\nnext line"]);
            assert!(listener.received.lock().await.iter().any(|event| matches!(event, Ev::SystemNotice { message, is_error: false } if message.contains(&task.task_id))));
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
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
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
            &handle.session_cwd,
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
                subagent_type: "general-purpose".into(),
                prompt: "do not replay this initial prompt".into(),
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
        let body = tokio::time::timeout(std::time::Duration::from_secs(5), captured_rx)
            .await
            .unwrap()
            .unwrap();
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
                    // Not a Local App workflow.
                    scope: None,
                },
                "workflow".to_string(),
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
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(branding::DOT_DIR),
        routing: Some(serde_json::json!({ "mobileEnabledProfiles": ["anthropic"] })),
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
                model: Some("anthropic/claude-sonnet-5".into()),
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
                Ev::ModelChanged { model } if model == "github-copilot/gpt-5.6-sol"
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
            .begin(&session_id, 16, "pause me".to_string(), None, Vec::new())
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
        assert_eq!(checkpoint.state, TurnRecoveryStateDto::PausedRecoverable);
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
            .begin(&session_id, 91, "paused".to_string(), None, Vec::new())
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
            .begin(&session_id, 92, "waiting".to_string(), None, Vec::new())
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
            assert_eq!(checkpoint.state, TurnRecoveryStateDto::Cancelled);
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
            .begin(&session_id, 93, "paused".to_string(), None, Vec::new())
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
            .begin(&session_id, 94, "terminal".to_string(), None, Vec::new())
            .expect("begin terminal turn");
        handle
            .durable_turns
            .cancel(&session_id, 94)
            .expect("cancel terminal fixture");
        handle
            .durable_turns
            .begin("other-session", 95, "foreign".to_string(), None, Vec::new())
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
            TurnRecoveryStateDto::PausedRecoverable
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
            TurnRecoveryStateDto::Running
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
            .begin(&session_id, 96, "pause".to_string(), None, Vec::new())
            .expect("begin turn");
        let turn = handle.reserve_turn(Some(96)).await.expect("reserve turn");
        let (owned_tx, owned_rx) = tokio::sync::oneshot::channel::<HashMap<String, String>>();
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
        let (unrelated_tx, mut unrelated_rx) =
            tokio::sync::oneshot::channel::<HashMap<String, String>>();
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
            TurnRecoveryStateDto::PausedRecoverable
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
            .begin(&session_id, 97, "cancel".to_string(), None, Vec::new())
            .expect("begin turn");
        let turn = handle.reserve_turn(Some(97)).await.expect("reserve turn");
        let (owned_tx, owned_rx) = tokio::sync::oneshot::channel::<HashMap<String, String>>();
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
        let (unrelated_tx, mut unrelated_rx) =
            tokio::sync::oneshot::channel::<HashMap<String, String>>();
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
            TurnRecoveryStateDto::Cancelled
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
            message: "Message accepted for task a12345678".to_string(),
            is_error: false,
        })
        .await;
    listener
        .on_event(Ev::CompactionCompleted {
            messages_before: 2,
            messages_after: 1,
            bytes_saved: 10,
            summary: "kept context".to_string(),
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
            message: "late notice".to_string(),
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
            text: "late".to_string(),
        })
        .await;
    listener
        .on_event(Ev::Error {
            kind: client::protocol::events::ErrorKindDto::Internal,
            message: "late error".to_string(),
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
            header: None,
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
            message: "unowned notice".to_string(),
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
async fn lifecycle_listener_emits_raw_event_before_replay_ack() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(crate::mobile::turn_durability::DurableTurnStore::new(
        temp.path().join("turns"),
    ));
    store
        .begin("session-a", 12, "hello".to_string(), None, Vec::new())
        .expect("begin");

    let inner = Arc::new(FakeListener::default());
    let active = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        super::ActiveTurn::new_owned(Some(12), "session-a".to_string(), 1),
    ))));
    let listener = super::TurnLifecycleListener::new_durable(inner.clone(), active, store);

    listener
        .on_event(Ev::TextDelta {
            text: "hello".to_string(),
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
        ] if text == "hello"
            && session_id == "session-a"
            && event_json.contains("\"text_delta\"")
    ));
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
        .begin("session-a", 21, "stop me".to_string(), None, Vec::new())
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
            display: None,
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
        .begin("session-a", 22, "done".to_string(), None, Vec::new())
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
        .begin("session-a", 23, "list".to_string(), None, Vec::new())
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
        .begin("session-a", 24, "ask".to_string(), None, Vec::new())
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
        TurnRecoveryStateDto::WaitingForUser
    );

    listener
        .on_event(Ev::AskUserQuestionResolved { request_id: 5 })
        .await;

    let checkpoint = store.load("session-a", 24).expect("load resolved");
    assert_eq!(
        checkpoint.state,
        TurnRecoveryStateDto::Running,
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
            .begin(&session_id, 41, "background".to_string(), None, Vec::new())
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
            })
            .await;

        assert!(
            result.is_ok(),
            "overlapping SendPrompt must queue: {result:?}"
        );
        assert_eq!(
            handle.message_queue.take_mid_turn_prompt().await.as_deref(),
            Some("pending guidance")
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

        let transition = handle.loop_transition.lock().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.submit(ClientCommand::AnswerAskUserQuestion {
                request_id: 1,
                answers: HashMap::from([("When?".to_string(), "Now".to_string())]),
            }),
        )
        .await
        .expect("answer must not wait for the transition lock")
        .expect("answer accepted");
        drop(transition);

        assert_eq!(
            response_rx.await.expect("broker response"),
            HashMap::from([("When?".to_string(), "Now".to_string())])
        );
        assert!(listener.received.lock().await.iter().any(|event| matches!(
            event,
            ClientEvent::AskUserQuestionResolved { request_id: 1 }
        )));
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
                    text: "queued raw loop".into(),
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
                AudioRecordingHandle("old-recording".into()),
            );
            handles.insert(
                unrelated_owner.clone(),
                AudioRecordingHandle("other-recording".into()),
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
            .insert(owner.clone(), AudioRecordingHandle("live-recording".into()));

        handle
            .submit(ClientCommand::SendPrompt {
                text: "finish this turn".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(71),
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
            g.check("Bash", &serde_json::json!({"command": "ls"})).await
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

/// WORKFLOWS: `submit(ResumeWorkflow)` adopts a workflow whose session
/// `adopt.json` checkpoint survives but whose registry row does not — the
/// evicted-row recovery path.
///
/// The checkpoint must name a PLUGIN workflow and claim a verbatim builtin
/// script whose on-disk bytes still hash to the recorded digest, so the
/// fixture uses a live plugin workflow id. The retired create coordinator's
/// recovery sidecars (`create-input.json` / `create-terminal.json`) went
/// with it — a create whose template never landed is re-planned through the
/// plan approval instead of being resumed — so this fixture carries the
/// checkpoint alone.
#[test]
fn submit_resume_workflow_recovers_a_plugin_workflow_from_its_checkpoint() {
    use sha2::{Digest, Sha256};
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
            let session = handle.active_session_id();
            let subagents = orchestrator::transcript_paths::subagents_dir(
                &handle.lingxi_home, &handle.session_cwd, &session,
            );
            let directory = subagents.join("workflows/wf_recover1");
            std::fs::create_dir_all(&directory).unwrap();
            let script_path = directory.join("build.js");
            let script = "export const meta = {name: 'local-app-use-test'}; return 1;";
            std::fs::write(&script_path, script).unwrap();
            std::fs::write(directory.join("journal.jsonl"), "").unwrap();
            let hash = format!("{:x}", Sha256::digest(script.as_bytes()));
            let workflow_id = crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID;
            let args = serde_json::json!({"app_id": "missing1"});
            let checkpoint = serde_json::json!({
                "taskId": "wrecover1", "workflowRunId": "wf_recover1", "workflowId": workflow_id,
                "scriptPath": script_path, "scriptSha256": hash, "scriptIsVerbatimBuiltin": true,
                "argsJson": args.to_string(), "description": "Interrupted use test", "transcriptDir": directory,
            });
            std::fs::write(subagents.parent().unwrap().join("adopt.json"),
                serde_json::json!({"workflows": [checkpoint]}).to_string()).unwrap();
            assert!(handle.inner.task_registry.get("wrecover1").await.is_none());
            let result = handle.submit(ClientCommand::ResumeWorkflow {task_id: "wrecover1".into()}).await;
            // The fixture intentionally has no app record: business validation
            // may reject launch, but task recovery must have completed first.
            assert!(!matches!(result, Err(ClientError::NotFound { .. })), "{result:?}");
            assert!(handle.inner.task_registry.get("wrecover1").await.is_some());
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
            session::jsonl::session_path(&handle.lingxi_home, &handle.session_cwd, &after_uuid);
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
            } if display.contains("unavailable in Chat mode")
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
        let path = session::jsonl::session_path(&handle.lingxi_home, &handle.session_cwd, expected);
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

// ── LOCAL-APPS (phase 1): handler-level tests (command in → state +
//    events out) over the real engine handle ─────────────────────────────

use client::protocol::commands::AppCreateModeDto;
use client::protocol::local_apps::{
    AppCreateOriginDto, AppErrorCodeDto, AppEventDto, AppRuntimeStateDto,
};

/// Drain and return every event delivered to the fake listener so far.
/// Event delivery is asynchronous, in two ordered stages: `AppService`
/// hands events to spawned emission tasks (commit → enqueue onto the
/// bridge's emission channel), and the bridge's single forwarder drains
/// that channel to the sink (enqueue → deliver). A bare take races both,
/// so the barrier is two-stage too: `flush_events` waits until everything
/// committed is ENQUEUED, then the queue flush waits until everything
/// enqueued is DELIVERED.
async fn drain_events(handle: &MobileEngineHandle, listener: &FakeListener) -> Vec<Ev> {
    if let Ok(service) = handle.local_apps() {
        service.flush_events().await;
    }
    handle.app_emissions.flush().await;
    std::mem::take(&mut *listener.received.lock().await)
}

fn apps_changed_rows(events: &[Ev]) -> Option<Vec<client::protocol::local_apps::AppRecordDto>> {
    events.iter().rev().find_map(|event| match event {
        Ev::AppsChanged { apps } => Some(apps.clone()),
        _ => None,
    })
}

/// This round trip covers the dynamic application list/details protocol;
/// the retired static-catalog exchange is intentionally absent.
#[test]
fn local_apps_create_and_details_round_trip_through_submit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: "Tracker".into(),
                origin: AppCreateOriginDto::Library,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp must announce AppCreated");
        let app_id = record.id;

        handle
            .submit(ClientCommand::GetAppDetails {
                app_id: app_id.clone(),
            })
            .await
            .expect("submit(GetAppDetails)");
        let events = drain_events(&handle, &listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::AppEvent {
                event: AppEventDto::AppDetailsChanged { details }
            } if details.app.id == app_id
                && matches!(details.runtime.state, AppRuntimeStateDto::Stopped)
        )));
    });
}

/// r2-critic-1: a disabled built-in plugin must refuse `CreateApp`
/// immediately with a typed failure instead of letting the app land and
/// dead-end later. Proves the gate can go RED (no plugin state check ->
/// `AppCreated` fires) and GREEN (checked -> refused, nothing created)
/// by exercising the same handler both ways within one test.
#[test]
fn create_app_is_refused_when_the_builtin_plugin_is_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        // Control: with the plugin in its default Loaded state, the
        // create succeeds -- this is what would happen for EVERY create
        // if the new gate above were silently miscompiled to a no-op.
        handle
            .submit(ClientCommand::CreateApp {
                name: "Control".into(),
                origin: AppCreateOriginDto::Library,
                brief: "plugin is enabled, this must succeed".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        assert!(
            created_row(&events).is_some(),
            "control create with the plugin enabled must succeed, got {events:?}"
        );

        handle
            .submit(ClientCommand::PluginCommand {
                command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                    plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                    enabled: false,
                },
            })
            .await
            .expect("submit(PluginCommand::SetEnabled false)");
        let _ = drain_events(&handle, &listener).await;

        handle
            .submit(ClientCommand::CreateApp {
                name: "Blocked".into(),
                origin: AppCreateOriginDto::Library,
                brief: "plugin is disabled, this must be refused".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: Some("req-plugin-disabled".into()),
            })
            .await
            .expect("submit(CreateApp) resolves Ok even for a domain refusal");
        let events = drain_events(&handle, &listener).await;
        assert!(
            created_row(&events).is_none(),
            "a disabled plugin must not let CreateApp mint a record, got {events:?}"
        );
        let (code, message, request_id) =
            first_failure(&events).expect("CreateApp must report a typed failure");
        assert_eq!(code, AppErrorCodeDto::NotYetAvailable);
        assert_eq!(request_id.as_deref(), Some("req-plugin-disabled"));
        assert!(
            message.contains("plugin"),
            "failure message should name the plugin as the cause, got {message:?}"
        );
    });
}

/// r2-critic-1 (coverage half): `handle_create_app`'s plugin gate covers
/// only `ClientCommand::CreateApp`. The agent-facing `LocalAppCreate` tool
/// is a SECOND live create entry point — `LocalAppTool` dispatches it
/// through `LocalAppsMcpTransport::call_host_operation("create", ..)`,
/// which reaches `AppService::create_app_…` without ever entering that
/// handler — and the local-apps MCP server stays connected while the
/// plugin is disabled (bootstrap registers it `disabled: false,
/// always_load: true`, and `set_builtin_plugin_enabled` only calls
/// `PluginManager::disable`). So this drives the TOOL, not the command,
/// against the very transport the engine wired, and asserts the same
/// plugin state refuses it with the same typed code and sentence.
#[test]
fn local_app_create_tool_is_refused_when_the_builtin_plugin_is_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());
    // THE production transport — the one `build_mobile_inner` attached the
    // availability probe to and `local_app_builtin_tools` hands to
    // `LocalAppTool`. A transport built by the test would prove nothing
    // about what the engine actually wired.
    let transport = handle.inner.local_apps_mcp.clone();

    handle.runtime().block_on(async {
            // Control: with the plugin in its default Loaded state the tool
            // create succeeds. Without this the refusal below could equally
            // come from an unattached host/service or a malformed payload.
            let created = transport
                .call_host_operation(
                    "create",
                    serde_json::json!({ "brief": "plugin is enabled, this must succeed" }),
                )
                .await
                .expect("LocalAppCreate dispatch");
            assert!(
                !created.is_error,
                "control create through the MCP tool must succeed, got {created:?}"
            );
            let structured = created
                .structured_content
                .clone()
                .expect("create returns structured content");
            assert!(
                structured["app"]["id"].as_str().is_some(),
                "control create must mint a record: {structured}"
            );

            handle
                .submit(ClientCommand::PluginCommand {
                    command: client::protocol::local_apps::PluginCommandDto::SetEnabled {
                        plugin_id: crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
                        enabled: false,
                    },
                })
                .await
                .expect("submit(PluginCommand::SetEnabled false)");
            let _ = drain_events(&handle, &listener).await;

            let refused = transport
                .call_host_operation(
                    "create",
                    serde_json::json!({ "brief": "plugin is disabled, this must be refused" }),
                )
                .await
                .expect("a domain refusal is a tool result, not a transport error");
            assert!(
                refused.is_error,
                "a disabled plugin must refuse the agent-facing create, got {refused:?}"
            );
            let text = refused.content.to_string();
            assert!(
                text.contains("not_yet_available"),
                "the refusal must carry the same typed code the ClientCommand                  path reports: {text}"
            );
            assert!(
                text.contains("the Local App plugin is disabled or unavailable"),
                "the refusal must say the same thing handle_create_app says: {text}"
            );

            // …and it must refuse BEFORE minting: the catalog still holds only
            // the control app.
            let listed = transport
                .call_host_operation("list", serde_json::json!({}))
                .await
                .expect("list dispatch")
                .structured_content
                .expect("list returns structured content");
            assert_eq!(
                listed["total"], 1,
                "a refused create must not mint a second record: {listed}"
            );
        });
}

/// v3 Phase 4: a library create pins an init session — the record gains
/// `init_session_id`, the anchor lands in the APP WORKSPACE's own
/// catalog, and `ListAppSessions` returns that row marked `Init`.
#[test]
fn create_app_pins_an_init_session_and_lists_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: "Tracker".into(),
                origin: AppCreateOriginDto::Library,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp announces AppCreated");
        let app_id = record.id.clone();
        // The create snapshot carries the new row; init-session pinning
        // arrives as one incremental record update.
        let pinned = events
            .iter()
            .find_map(|event| match event {
                Ev::AppEvent {
                    event: AppEventDto::AppRecordChanged { record },
                } if record.id == app_id => Some(record.clone()),
                _ => None,
            })
            .expect("an incremental record update carries the pin");
        let init_id = pinned
            .init_session_id
            .clone()
            .expect("library create pins an empty init anchor");
        let settings_path = tmp
            .path()
            .join("apps")
            .join(&app_id)
            .join("workspace")
            .join(".lingxi/settings.local.json");
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path)
                .expect("CreateApp seeds workspace permission settings"),
        )
        .expect("workspace permission settings are valid JSON");
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!([
                "Read(./**)",
                "Edit(./**)",
                "LocalAppLogs",
                "LocalAppBuild",
                "LocalAppRuntime"
            ])
        );
        assert!(
            !tmp.path()
                .join("apps")
                .join(&app_id)
                .join("workspace")
                .join(&init_id)
                .join(".lingxi/settings.local.json")
                .exists(),
            "session ids are catalog keys, not workspace path components"
        );

        handle
            .submit(ClientCommand::ListAppSessions {
                app_id: app_id.clone(),
                offset: None,
                limit: None,
            })
            .await
            .expect("submit(ListAppSessions)");
        let events = drain_events(&handle, &listener).await;
        let (sessions, next_offset) = events
            .iter()
            .find_map(|event| match event {
                Ev::AppSessionsChanged {
                    app_id: got,
                    sessions,
                    next_offset,
                } if *got == app_id => Some((sessions.clone(), *next_offset)),
                _ => None,
            })
            .expect("ListAppSessions replies with AppSessionsChanged");
        assert_eq!(next_offset, None, "one anchor row — no further pages");
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        assert_eq!(sessions[0].uuid, init_id);
        assert_eq!(
            sessions[0].kind,
            client::protocol::local_apps::AppSessionKindDto::Init
        );
        assert_eq!(sessions[0].message_count, 0, "an anchor is empty");
    });
}

/// r1-backlog-engine-create-10: an app remembers the cwd it was created
/// from, and the fork must use THAT catalog — not whatever cwd the caller
/// happens to be anchored to now.
///
/// This is the boot-repair case made concrete: the backfill sweep calls
/// `mint_app_init_session` with the CONNECTION's cwd, which for an app
/// created from a different scope names a catalog the source conversation
/// was never in. The fork then found nothing and silently degraded to an
/// empty anchor, losing the user's real transcript.
#[tokio::test]
async fn mint_forks_from_the_recorded_origin_cwd_not_the_callers_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lingxi_home = tmp.path().join(".claude");
    let origin_cwd = tmp.path().join("origin").to_string_lossy().to_string();
    let other_cwd = tmp
        .path()
        .join("somewhere-else")
        .to_string_lossy()
        .to_string();
    std::fs::create_dir_all(&origin_cwd).unwrap();
    std::fs::create_dir_all(&other_cwd).unwrap();
    let data_root = tmp.path().to_path_buf();

    // The source conversation exists ONLY in the origin cwd's catalog.
    let source_uuid = uuid::Uuid::new_v4();
    let src_path = orchestrator::transcript_paths::main_transcript_path(
        &lingxi_home,
        &origin_cwd,
        &source_uuid.to_string(),
    );
    std::fs::create_dir_all(src_path.parent().unwrap()).unwrap();
    std::fs::write(
        &src_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "uuid": "33333333-3333-3333-3333-333333333333",
                "parentUuid": None::<String>,
                "sessionId": source_uuid.to_string(),
                "timestamp": "2026-09-04T12:00:00.000Z",
                "cwd": origin_cwd,
                "version": "0.0.0",
                "message": { "role": "user", "content": "remember where I came from" },
            })
        ),
    )
    .unwrap();

    let mut record = local_apps::AppState::create(
        "zz9origin".into(),
        "来源".into(),
        "forks from its origin scope".into(),
        Some(source_uuid.to_string()),
        1,
    )
    .record;
    record.origin_cwd = Some(origin_cwd.clone());
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    // The caller is anchored somewhere else entirely — the sweep's case.
    let init_id = super::mint_app_init_session(&lingxi_home, &other_cwd, &data_root, fs, &record)
        .await
        .expect("mint");

    let workspace_cwd = crate::mobile::local_apps_host::canonical_cwd_string(
        &data_root.join(&record.workspace_rel),
    );
    let forked = std::fs::read_to_string(orchestrator::transcript_paths::main_transcript_path(
        &lingxi_home,
        &workspace_cwd,
        &init_id,
    ))
    .expect("the minted session lives in the app catalog");
    assert!(
        forked.contains("remember where I came from"),
        "the fork must follow the app's RECORDED origin catalog, not the caller's cwd; \
             an empty anchor here is the silent transcript loss this fixes: {forked}"
    );
}

/// v3 Phase 4: a chat-origin create FORKS the source conversation into
/// the app workspace catalog — history follows, entries re-root on the
/// workspace cwd, and the source session file is untouched. Direct test
/// of `mint_app_init_session` (the submit path needs a live turn to put
/// messages in the source session; the fork mechanics are what matter).
#[tokio::test]
async fn chat_origin_mint_forks_the_source_conversation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lingxi_home = tmp.path().join(".claude");
    let source_cwd = tmp.path().to_string_lossy().to_string();
    let data_root = tmp.path().to_path_buf();
    // Seed a two-message source session in the source cwd's catalog.
    let source_uuid = uuid::Uuid::new_v4();
    let src_path = orchestrator::transcript_paths::main_transcript_path(
        &lingxi_home,
        &source_cwd,
        &source_uuid.to_string(),
    );
    std::fs::create_dir_all(src_path.parent().unwrap()).unwrap();
    let line = |uuid: &str, parent: Option<&str>, text: &str| {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "sessionId": source_uuid.to_string(),
            "timestamp": "2026-08-09T12:00:00.000Z",
            "cwd": source_cwd,
            "version": "0.0.0",
            "message": { "role": "user", "content": text },
        })
        .to_string()
    };
    std::fs::write(
        &src_path,
        format!(
            "{}\n{}\n",
            line("11111111-1111-1111-1111-111111111111", None, "make an app"),
            line(
                "22222222-2222-2222-2222-222222222222",
                Some("11111111-1111-1111-1111-111111111111"),
                "it tracks habits",
            ),
        ),
    )
    .unwrap();
    let source_before = std::fs::read_to_string(&src_path).unwrap();

    let record = local_apps::AppState::create(
        "zz9plural".into(),
        "习惯".into(),
        "track habits".into(),
        Some(source_uuid.to_string()),
        1,
    )
    .record;
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let init_id = super::mint_app_init_session(&lingxi_home, &source_cwd, &data_root, fs, &record)
        .await
        .expect("mint forks");

    // Spell the catalog directory the way `mint_app_init_session` does.
    // A raw `to_string_lossy()` used to match only because the old
    // `canonical_cwd_string` fell back to the raw path when the workspace
    // leaf did not exist yet — which is exactly the bug r1-engine-core-010
    // fixed, since mint (leaf present) and a later cleanup (leaf gone) then
    // disagreed on where the catalog lived. Now it always canonicalises the
    // nearest existing ancestor, so on a symlink-split platform this is
    // `/private/var/...` where the raw join says `/var/...`.
    let workspace_cwd = crate::mobile::local_apps_host::canonical_cwd_string(
        &data_root.join(&record.workspace_rel),
    );
    let fork_path = orchestrator::transcript_paths::main_transcript_path(
        &lingxi_home,
        &workspace_cwd,
        &init_id,
    );
    let forked = std::fs::read_to_string(&fork_path).expect("fork lives in the app catalog");
    assert!(forked.contains("it tracks habits"), "history followed");
    assert!(
        forked.contains(&format!("\"cwd\":{}", serde_json::json!(workspace_cwd))),
        "entries re-root on the workspace cwd: {forked}"
    );
    assert_eq!(
        std::fs::read_to_string(&src_path).unwrap(),
        source_before,
        "the source session must be untouched"
    );
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

/// r1-engine-core-002: `mint_app_init_session`'s empty-anchor branch
/// makes TWO writes to the same freshly-created transcript file —
/// `append_mobile_empty_session` then `append_session_mode`. Neither
/// `map_err` arm removed the file on failure, so a mint that got past
/// the first write and failed on the second stranded a real transcript
/// file on disk forever (the caller only ever sees the `Err` string).
/// This forces exactly that: the SECOND write fails, and the assertion
/// is that the file is actually gone afterward, not that an `Err` came
/// back.
#[tokio::test]
async fn mint_app_init_session_cleans_up_orphan_transcript_on_partial_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lingxi_home = tmp.path().join(".claude");
    let source_cwd = tmp.path().to_string_lossy().to_string();
    let data_root = tmp.path().to_path_buf();
    let record = local_apps::AppState::create(
        "orphan-app".into(),
        "孤儿".into(),
        "no source conversation, takes the empty-anchor branch".into(),
        None,
        1,
    )
    .record;

    let inner: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(FlakyAppendFs {
        inner,
        target_suffix: ".jsonl".to_string(),
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_on_call: 2,
    });

    let result =
        super::mint_app_init_session(&lingxi_home, &source_cwd, &data_root, fs, &record).await;
    let error = result.expect_err("the planted 2nd append failure must surface");
    assert!(
        error.contains("persist app init session mode"),
        "got {error:?}"
    );

    let workspace_cwd = super::canonical_cwd_string(&data_root.join(&record.workspace_rel));
    let catalog_dir = lingxi_home
        .join("projects")
        .join(session::jsonl::path::project_dir_name(&workspace_cwd));
    let stray_files: Vec<_> = std::fs::read_dir(&catalog_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert!(
        stray_files.is_empty(),
        "a half-completed mint must not strand its transcript file, found {stray_files:?}"
    );
}

/// r1-engine-core-002, FORK arm. The gate above only reaches
/// `mint_app_init_session`'s empty-anchor branch (its fixture record has
/// no `conversation_id`). The chat-origin branch strands the more
/// expensive artifact -- a full transcript COPY that
/// `create_branch_to_cwd` already wrote -- and derives its cleanup path
/// through a second spelling (`remove_app_session_file`) that could drift
/// from `main_transcript_path`. This forces that exact arm: the fork body
/// lands (`create_branch_to_cwd` writes it with `tokio::fs::write`, which
/// this double does not intercept), so the FIRST `append_file_with_mode`
/// on the forked file is the `session-mode` append -- planted to fail.
#[tokio::test]
async fn mint_app_init_session_cleans_up_a_forked_transcript_on_partial_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lingxi_home = tmp.path().join(".claude");
    let source_cwd = tmp.path().to_string_lossy().to_string();
    let data_root = tmp.path().to_path_buf();

    let source_session_id = uuid::Uuid::new_v4();
    let source_path = orchestrator::transcript_paths::main_transcript_path(
        &lingxi_home,
        &source_cwd,
        &source_session_id.to_string(),
    );
    std::fs::create_dir_all(source_path.parent().expect("parent")).expect("source catalog");
    std::fs::write(
        &source_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "uuid": "22222222-2222-2222-2222-222222222222",
                "parentUuid": null,
                "sessionId": source_session_id.to_string(),
                "timestamp": "2026-08-09T12:00:00.000Z",
                "cwd": source_cwd,
                "version": "0.0.0",
                "message": { "role": "user", "content": "the chat this app forks from" },
            })
        ),
    )
    .expect("seed the source conversation");

    let record = local_apps::AppState::create(
        "fork-app".into(),
        "分叉".into(),
        "has a source conversation, takes the chat-origin fork branch".into(),
        Some(source_session_id.to_string()),
        1,
    )
    .record;

    let inner: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(FlakyAppendFs {
        inner,
        target_suffix: ".jsonl".to_string(),
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_on_call: 1,
    });

    let result =
        super::mint_app_init_session(&lingxi_home, &source_cwd, &data_root, fs, &record).await;
    let error = result.expect_err("the planted session-mode append failure must surface");
    assert!(
        error.contains("persist app init session mode"),
        "the FORK arm must be the arm that failed -- a degraded empty anchor would have \
             failed its first write with \"anchor app init session\" instead, got {error:?}"
    );

    let workspace_cwd = super::canonical_cwd_string(&data_root.join(&record.workspace_rel));
    let catalog_dir = lingxi_home
        .join("projects")
        .join(session::jsonl::path::project_dir_name(&workspace_cwd));
    let stray_files: Vec<_> = std::fs::read_dir(&catalog_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert!(
        stray_files.is_empty(),
        "a half-completed FORK must not strand the transcript copy it already wrote, \
             found {stray_files:?}"
    );
}

/// r1-engine-core-013: when the promote-rename fails, the directory
/// holding the pinned init session must be merged FIRST so the
/// first-writer-wins collision loop right after it protects that copy,
/// not whichever unrelated drifted directory already sat in the vector.
/// Before the fix this pushed to the back (`drifted.push(base)`), which
/// would have failed this assertion (`drifted[0]` would have been
/// `sibling`, not `base`).
#[test]
fn requeue_failed_catalog_promotion_puts_the_pin_holder_first() {
    let sibling = std::path::PathBuf::from("/tmp/apps/sibling-apps-x-workspace");
    let base = std::path::PathBuf::from("/tmp/apps/base-holds-the-pin-apps-x-workspace");
    let mut drifted = vec![sibling.clone()];

    super::requeue_failed_catalog_promotion(base.clone(), &mut drifted);

    assert_eq!(
        drifted,
        vec![base, sibling],
        "the pin holder must be merged before any other drifted directory"
    );
}

/// r1-failure-paths-012: a pin-less record is not necessarily an empty
/// shell -- a create that minted a REAL conversation and then failed
/// before `set_init_session` leaves exactly this state. Seeds a
/// non-empty session file directly under the app's workspace catalog
/// with no pin ever set, runs the real boot sweep, and asserts the
/// record gets pinned to THAT conversation -- not a freshly minted
/// empty anchor stacked next to it.
#[tokio::test]
async fn boot_backfill_adopts_an_existing_unpinned_conversation_instead_of_minting() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let backfill_home = tmp.path().join(".claude");
    let backfill_root = tmp.path().to_path_buf();
    let backfill_cwd = tmp.path().to_string_lossy().to_string();
    let backfill_fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let backfill_service = Arc::new(
        local_apps::AppService::load(
            tmp.path(),
            Arc::new(local_apps::test_support::FixedClock::new(1)),
            Arc::new(local_apps::NoopAppEventObserver),
        )
        .await
        .expect("load app service"),
    );
    let record = backfill_service
        .create_app(Some("领养"), "adopt an orphaned conversation", None)
        .await
        .expect("seed record");
    assert!(
        record.init_session_id.is_none(),
        "fixture must start unpinned"
    );

    let workspace_cwd = super::canonical_cwd_string(&backfill_root.join(&record.workspace_rel));
    let orphan_session_id = uuid::Uuid::new_v4();
    let session_path = orchestrator::transcript_paths::main_transcript_path(
        &backfill_home,
        &workspace_cwd,
        &orphan_session_id.to_string(),
    );
    std::fs::create_dir_all(session_path.parent().expect("parent")).expect("catalog dir");
    std::fs::write(
        &session_path,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "uuid": "11111111-1111-1111-1111-111111111111",
                "parentUuid": null,
                "sessionId": orphan_session_id.to_string(),
                "timestamp": "2026-08-09T12:00:00.000Z",
                "cwd": workspace_cwd,
                "version": "0.0.0",
                "message": { "role": "user", "content": "orphaned by a half-completed create" },
            })
        ),
    )
    .expect("seed orphan conversation");

    let broker =
        super::LocalAppsHostBroker::new(backfill_root.clone(), MockSink::arc(), None, false, None);

    super::run_app_boot_backfill_sweep(
        backfill_home.clone(),
        backfill_cwd,
        backfill_root,
        backfill_fs.clone(),
        backfill_service.clone(),
        broker,
    )
    .await;

    let after = backfill_service
        .record(&record.id)
        .await
        .expect("record after sweep");
    assert_eq!(
        after.init_session_id.as_deref(),
        Some(orphan_session_id.to_string().as_str()),
        "boot backfill must adopt the existing non-empty conversation, not mint a fresh \
             empty anchor over it, got {after:?}"
    );

    let rows =
        session::jsonl::list_recent_sessions(&backfill_home, &workspace_cwd, 50, backfill_fs)
            .await
            .expect("list sessions after sweep");
    assert_eq!(
        rows.len(),
        1,
        "adoption must not ALSO mint a second, empty anchor next to it: {rows:?}"
    );
}

/// v3 Phase 4: a cross-workspace `NewSession`/`ResumeSession` cwd is
/// REJECTED (the old behavior silently ignored it — a client could
/// believe a workspace switch happened). A matching cwd still works.
#[test]
fn new_session_rejects_a_foreign_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, _listener) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        let err = handle
            .submit(ClientCommand::NewSession {
                cwd: Some("/somewhere/else".into()),
                model: None,
            })
            .await
            .expect_err("a foreign cwd must be rejected");
        assert!(
            matches!(err, ClientError::Rejected { ref message }
                    if message.contains("does not match this source's cwd")),
            "{err:?}"
        );
        let err = handle
            .submit(ClientCommand::ResumeSession {
                session_id: uuid::Uuid::new_v4().to_string(),
                cwd: Some("/somewhere/else".into()),
            })
            .await
            .expect_err("resume with a foreign cwd must be rejected");
        assert!(
            matches!(err, ClientError::Rejected { ref message }
                    if message.contains("does not match this source's cwd")),
            "{err:?}"
        );
        // The matching cwd is still honored.
        handle
            .submit(ClientCommand::NewSession {
                cwd: Some(tmp.path().to_string_lossy().to_string()),
                model: None,
            })
            .await
            .expect("the source's own cwd is honored");
    });
}

/// PINS the Task 11 fix: `ClientCommand::CreateApp` now carries a real
/// `brief` field, and `handle_create_app` persists the CALLER-SUPPLIED
/// brief — not `name` doubling as the brief (the deliberate placeholder
/// this test used to pin, `create_app_persists_name_as_brief_until_
/// task_11_adds_a_real_one`, until this task landed). All three LLM
/// stages read `AppRecord.brief`, so a client-created app must author
/// its questionnaire from the caller's real spec, not a bare display
/// name.
///
/// Mirrors `create_persists_the_caller_supplied_brief_and_does_not_
/// overwrite_a_supplied_name` in `local_apps_mcp.rs` (Task 10's side of
/// this same fix): `name` and `brief` are asserted UNEQUAL and both
/// checked, so this cannot pass by conflating them back together. The
/// `NAME` fixture stays longer than `AppService::create_app`'s 24-char
/// placeholder cut so a regression to `create_app(None, brief, ..)`
/// (`name` silently dropped) is visible too: it would come back as the
/// brief's own 24-char prefix instead of `NAME`.
#[test]
fn create_app_persists_the_caller_supplied_brief_and_does_not_overwrite_a_supplied_name() {
    const NAME: &str = "Habit Tracker Deluxe Edition";
    const BRIEF: &str = "一个记事本 app，用来跟踪每天的习惯打卡";
    assert!(
        NAME.chars().count() > 24,
        "test fixture must exceed the placeholder cut to be meaningful"
    );
    assert_ne!(
        NAME, BRIEF,
        "name and brief must be distinct fixtures so the test cannot pass by \
             conflating them"
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: NAME.into(),
                origin: AppCreateOriginDto::Library,
                brief: BRIEF.into(),
                git_enabled: true,
                workflow_model: Some("deepseek/deepseek-flash".into()),
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp must announce AppCreated");
        let app_id = record.id;

        let service = handle.local_apps().expect("local-apps service");
        let record = service.record(&app_id).await.expect("record");
        assert_eq!(
            record.name, NAME,
            "a caller-supplied name must not be silently overwritten"
        );
        assert_eq!(
            record.brief, BRIEF,
            "the brief the caller supplied is the brief that gets stored — not the \
                 name, not empty, not anything else"
        );
        assert_eq!(
            record.workflow_model.as_deref(),
            Some("deepseek/deepseek-flash"),
            "CreateApp must persist the selected workflow model as structured metadata"
        );
    });
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

#[test]
fn local_apps_runtime_and_checkpoint_commands_report_real_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: "Board".into(),
                origin: AppCreateOriginDto::Chat,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: Some("conv-7".into()),
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp must announce AppCreated");
        assert_eq!(
            record.conversation_id.as_deref(),
            Some("conv-7"),
            "a chat-origin create keeps the conversation binding"
        );
        let app_id = record.id;

        // Persisted capability grants are revocable from the native
        // permissions page without exposing permissions.json to clients.
        let layout = local_apps::AppLayout::new(tmp.path(), app_id.clone()).unwrap();
        let mut permissions = local_apps::AppPermissions::default();
        permissions.grant(local_apps::AppCapability::DataMutation);
        permissions.grant_domain("api.example.com").unwrap();
        local_apps::save_permissions(&layout, &permissions).unwrap();
        handle
            .submit(ClientCommand::ResetAppPermissions {
                app_id: app_id.clone(),
            })
            .await
            .expect("submit(ResetAppPermissions)");
        let reset = local_apps::load_permissions(&layout).unwrap();
        assert!(reset.always_allowed_capabilities.is_empty());
        assert!(reset.always_allowed_domains.is_empty());
        assert!(
            reset.grant_epoch > permissions.grant_epoch,
            "reset must advance the grant epoch to invalidate prior leases"
        );

        // A record without generated build output fails honestly and never
        // reports a false running state.
        handle
            .submit(ClientCommand::StartApp {
                app_id: app_id.clone(),
            })
            .await
            .expect("submit(StartApp)");
        let events = drain_events(&handle, &listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::AppOperationFailed { app_id: Some(id), .. } if *id == app_id
        )));
        let service = handle.local_apps().expect("local-apps service");
        assert_ne!(
            service.runtime_record(&app_id).await.unwrap().state,
            local_apps::AppRuntimeState::Running,
            "a missing build must never be surfaced as running"
        );

        // A missing app is not_found, not not_yet_available.
        handle
            .submit(ClientCommand::StartApp {
                app_id: "beadfeed".into(),
            })
            .await
            .expect("submit(StartApp missing)");
        let events = drain_events(&handle, &listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::AppOperationFailed {
                code: AppErrorCodeDto::NotFound,
                ..
            }
        )));

        // Checkpoint lists now use the explicit snapshot event, including
        // an empty list for a new app.
        handle
            .submit(ClientCommand::ListAppCheckpoints {
                app_id: app_id.clone(),
            })
            .await
            .expect("submit(ListAppCheckpoints)");
        let events = drain_events(&handle, &listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::AppEvent {
                event: AppEventDto::AppCheckpointsChanged { app_id: id, checkpoints }
            } if *id == app_id && checkpoints.is_empty()
        )));
        handle
            .submit(ClientCommand::ListAppCheckpoints {
                app_id: "beadfeed".into(),
            })
            .await
            .expect("submit(ListAppCheckpoints missing)");
        let events = drain_events(&handle, &listener).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ev::AppOperationFailed {
                code: AppErrorCodeDto::NotFound,
                ..
            }
        )));
    });
}

#[test]
fn local_apps_state_survives_engine_rebuild_and_delete_removes_the_app() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());
    let app_id = handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: "Persist".into(),
                origin: AppCreateOriginDto::Library,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp must announce AppCreated");

        record.id
    });
    // The store lives at the per-profile data root (`<root>/apps/…`) —
    // `test_config` roots `lingxi_home` under the temp dir, so its parent
    // (the temp root) is the profile root.
    assert!(tmp
        .path()
        .join("apps")
        .join(&app_id)
        .join("runtime.json")
        .is_file());
    drop(handle);

    // A brand-new engine over the same root rebuilds from disk alone.
    let (handle, listener) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::ListApps)
            .await
            .expect("submit(ListApps)");
        let events = drain_events(&handle, &listener).await;
        let apps = apps_changed_rows(&events).expect("ListApps replies with AppsChanged");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, app_id);
        handle
            .submit(ClientCommand::DeleteApp {
                app_id: app_id.clone(),
            })
            .await
            .expect("submit(DeleteApp)");
        let events = drain_events(&handle, &listener).await;
        assert!(events
            .iter()
            .any(|event| matches!(event, Ev::AppsChanged { apps } if apps.is_empty())));
    });
    assert!(
        !tmp.path().join("apps").join(&app_id).exists(),
        "DeleteApp must remove the app directory"
    );
}

#[test]
fn initial_app_cold_boot_exposes_only_its_enabled_managed_mcp() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("seed runtime");
    let (app_id, workspace) = seed_runtime.block_on(async {
        let service = local_apps::AppService::load(
            tmp.path().to_path_buf(),
            Arc::new(platform_posix_minimal::PosixClock::new()),
            Arc::new(local_apps::NoopAppEventObserver),
        )
        .await
        .expect("seed app service");
        let record = service
            .create_app(Some("Published"), "cold-boot MCP", None)
            .await
            .expect("seed app");
        let layout =
            local_apps::AppLayout::new(tmp.path(), record.id.clone()).expect("seed app layout");
        let definition = lingxi_core::host::McpToolDefinitionDto::new(
            "read_value",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        );
        let catalog = serde_json::json!({
            "appId": record.id.clone(),
            "buildId": "build-cold",
            "tools": [{
                "definition": definition,
                "ceiling": "allow"
            }],
            "execution": []
        });
        let catalog_sha256 = local_apps::hash_mcp_catalog(catalog.clone()).expect("catalog digest");
        local_apps::save_mcp_catalog(&layout, &catalog_sha256, &catalog).expect("save catalog");
        let mut manifest = local_apps::load_manifest(&layout).expect("seed manifest");
        manifest.revision = manifest.revision.max(1);
        let profile_sha256 = "2".repeat(64);
        manifest.surface = Some(local_apps::AppSurface::Dom);
        manifest.runtime_profile = Some(local_apps::AppRuntimeProfileBinding {
            family: local_apps::AppRuntimeProfile::ReactDom,
            revision: 1,
            contract_sha256: profile_sha256.clone(),
        });
        manifest.dependency_snapshot = Some(local_apps::AppDependencySnapshot {
            requested_sha256: "3".repeat(64),
            package_sha256: "4".repeat(64),
            lockfile_sha256: "5".repeat(64),
            dependency_tree_sha256: "6".repeat(64),
            sbom_sha256: "7".repeat(64),
            toolchain_key: "pnpm@test/node@test".into(),
            verified_profile_contract_sha256: profile_sha256.clone(),
        });
        manifest.template_origin = Some(local_apps::AppTemplateOrigin {
            plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: "react-dom-r4".into(),
            template_sha256: profile_sha256,
        });
        manifest.active_mcp_catalog = Some(local_apps::AppMcpCatalogRef {
            build_id: "build-cold".into(),
            manifest_revision: manifest.revision,
            authoring_revision: 1,
            user_goal_sha256: "0".repeat(64),
            proposal_sha256: "0".repeat(64),
            approval_contract_sha256: "0".repeat(64),
            tool_surface_sha256: "1".repeat(64),
            catalog_sha256,
            mcp_verification_sha256: "0".repeat(64),
        });
        local_apps::save_manifest(&layout, &manifest).expect("publish catalog pointer");
        local_apps::save_mcp_settings(
            &layout,
            &local_apps::AppMcpSettings {
                enabled: true,
                enabled_tools: vec!["read_value".into()],
                ..Default::default()
            },
            None,
        )
        .expect("enable published MCP");
        (record.id, tmp.path().join(layout.workspace_rel()))
    });
    drop(seed_runtime);

    // The authoring plugin can be disabled independently of an app MCP
    // the user already published and enabled.
    let settings_dir = tmp.path().join(branding::DOT_DIR);
    std::fs::create_dir_all(&settings_dir).expect("settings directory");
    std::fs::write(
        settings_dir.join("settings.json"),
        serde_json::json!({
            "enabledPlugins": { "lingxi-local-app": false }
        })
        .to_string(),
    )
    .expect("disabled authoring plugin setting");

    let mut cfg = test_config(tmp.path());
    cfg.cwd = workspace;
    let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
    assert_eq!(
        handle.skill_count(),
        0,
        "the authoring plugin must remain disabled on this cold boot"
    );
    let expected = format!("mcp__local_app_{app_id}__read_value");
    handle.runtime().block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if handle.inner.orchestrator.tool_names().contains(&expected) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "initial Local App cold boot did not expose {expected:?}; tools={:?}",
                handle.inner.orchestrator.tool_names()
            )
        });
    });
}

/// r1-engine-core-015 / r1-backlog-engine-create-06: an app's transcripts
/// do NOT live under `apps/<id>` — they live in the ordinary per-cwd
/// catalog `<lingxi_home>/projects/<sanitize(workspace)>/`, which
/// `AppService::delete_app` cannot see. Without an explicit removal every
/// session this host minted for the app — for a chat-origin app, a full
/// FORK of the user's own conversation — outlives the app forever.
#[test]
fn delete_app_removes_the_apps_session_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());
    let (app_id, catalog_dir) = handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: "Catalog".into(),
                origin: AppCreateOriginDto::Library,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp must announce AppCreated");
        let app_id = record.id;
        // Derived independently of the production helper, and while the
        // workspace still exists, so this test cannot agree with the code
        // by simply calling the same function.
        let workspace_cwd = crate::mobile::local_apps_host::canonical_cwd_string(
            &tmp.path().join("apps").join(&app_id).join("workspace"),
        );
        let catalog_dir = tmp
            .path()
            .join(branding::DOT_DIR)
            .join("projects")
            .join(session::jsonl::path::project_dir_name(&workspace_cwd));
        (app_id, catalog_dir)
    });

    // Vacuity guard: with no transcript on disk the post-delete assertion
    // would pass against an app that never had a catalog at all.
    let minted: Vec<_> = std::fs::read_dir(&catalog_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert!(
        !minted.is_empty(),
        "CreateApp must have minted an init session under {}",
        catalog_dir.display()
    );

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::DeleteApp {
                app_id: app_id.clone(),
            })
            .await
            .expect("submit(DeleteApp)");
        let events = drain_events(&handle, &listener).await;
        assert!(events
            .iter()
            .any(|event| matches!(event, Ev::AppsChanged { apps } if apps.is_empty())));
    });

    assert!(
        !catalog_dir.exists(),
        "DeleteApp must remove the app's session catalog; {} survived with {:?}",
        catalog_dir.display(),
        std::fs::read_dir(&catalog_dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );
}

/// W4: the runtime seam (the broker's runtime manager drives it) must
/// reach the client through the same lowered wire path as command
/// replies — field-exact.
#[test]
fn local_apps_runtime_seam_delivers_field_exact_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::CreateApp {
                name: "Telemetry".into(),
                origin: AppCreateOriginDto::Library,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp)");
        let events = drain_events(&handle, &listener).await;
        let (record, _) = created_row(&events).expect("CreateApp must announce AppCreated");
        let app_id = record.id;
        let service = handle.local_apps().expect("local-apps service");

        // Runtime record: stopped -> starting (pins the port)…
        service
            .update_runtime_record(
                &app_id,
                local_apps::AppRuntimeState::Starting,
                Some(3001),
                Some(77),
                None,
            )
            .await
            .expect("update_runtime_record(starting)");
        // …then starting -> failed, carrying the failure detail.
        service
            .update_runtime_record(
                &app_id,
                local_apps::AppRuntimeState::Failed,
                None,
                None,
                Some("dev server exited: code 1".into()),
            )
            .await
            .expect("update_runtime_record(failed)");
        let events = drain_events(&handle, &listener).await;
        let starting_at = position_of(&events, "AppRuntimeChanged(starting)", |event| {
            matches!(
                event,
                Ev::AppRuntimeChanged {
                    app_id: id,
                    state: AppRuntimeStateDto::Starting,
                    last_error: None,
                    ..
                } if *id == app_id
            )
        });
        let failed_at = position_of(&events, "AppRuntimeChanged(failed)", |event| {
            matches!(
                event,
                Ev::AppRuntimeChanged {
                    app_id: id,
                    state: AppRuntimeStateDto::Failed,
                    last_error: Some(last_error),
                    ..
                } if *id == app_id && last_error == "dev server exited: code 1"
            )
        });
        assert!(
            starting_at < failed_at,
            "runtime transitions must arrive in commit order (got {events:?})"
        );
    });
}

/// A corrupt on-disk apps store must DEGRADE, never brick: the engine
/// still builds (chat is unaffected), `local_apps()` returns the boot
/// error, and every app command surfaces it as a typed
/// `AppOperationFailed { code: storage_corrupt }` — the Err branch of the
/// handle's `local_apps` Result and `local_apps_or_report`'s emit path.
#[test]
fn local_apps_corrupt_store_degrades_but_never_bricks_the_engine() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("apps")).expect("apps dir");
    std::fs::write(tmp.path().join("apps/index.json"), "{ not json").expect("plant corrupt index");

    // The engine still builds over the corrupt store…
    let (handle, listener) = build_submit_handle(tmp.path());
    handle.runtime().block_on(async {
        // …the boot-time load error is held on the handle…
        let err = match handle.local_apps() {
            Err(error) => error,
            Ok(_) => panic!("a corrupt store must surface as the boot error"),
        };
        assert_eq!(err.code(), local_apps::AppErrorCode::StorageCorrupt);

        // …and every app command reports it typed instead of hanging or
        // pretending an empty store.
        handle
            .submit(ClientCommand::ListApps)
            .await
            .expect("submit(ListApps) must not be a transport error");
        handle
            .submit(ClientCommand::CreateApp {
                name: "Habit Tracker".into(),
                origin: AppCreateOriginDto::Library,
                brief: "a test app".into(),
                git_enabled: true,
                workflow_model: None,
                conversation_id: None,
                surface: None,
                mode: AppCreateModeDto::Shell,
                request_id: None,
            })
            .await
            .expect("submit(CreateApp) must not be a transport error");
        let events = drain_events(&handle, &listener).await;
        let failures = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Ev::AppOperationFailed {
                        code: AppErrorCodeDto::StorageCorrupt,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            failures, 2,
            "each app command must emit the typed boot failure: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ev::AppsChanged { .. })),
            "a corrupt store must never masquerade as an (empty) app list"
        );
    });
}

// ── Create-flow: `CreateApp{mode}` forks shell vs scaffolded ───────────

/// Submit one `CreateApp` and return every event it produced.
///
/// Deliberately takes `name` as a `&str` (never an `Option`): the wire
/// field is a required `String` and the "+" button sends `""`, so a helper
/// that offered `None` would test a payload no client can send.
#[allow(clippy::too_many_arguments)]
async fn submit_create(
    handle: &MobileEngineHandle,
    listener: &FakeListener,
    name: &str,
    brief: &str,
    surface: Option<client::protocol::local_apps::AppSurfaceDto>,
    mode: AppCreateModeDto,
    request_id: Option<&str>,
) -> Vec<Ev> {
    handle
        .submit(ClientCommand::CreateApp {
            name: name.into(),
            origin: AppCreateOriginDto::Library,
            brief: brief.into(),
            git_enabled: true,
            workflow_model: None,
            conversation_id: None,
            surface,
            mode,
            request_id: request_id.map(str::to_string),
        })
        .await
        .expect("submit(CreateApp) must not be a transport error");
    drain_events(handle, listener).await
}

/// The `AppCreated` row (record + correlation key) from a create's events.
fn created_row(
    events: &[Ev],
) -> Option<(client::protocol::local_apps::AppRecordDto, Option<String>)> {
    events.iter().find_map(|event| match event {
        Ev::AppEvent {
            event: AppEventDto::AppCreated { record, request_id },
        } => Some((record.clone(), request_id.clone())),
        _ => None,
    })
}

/// The first `AppOperationFailed` in a create's events.
fn first_failure(events: &[Ev]) -> Option<(AppErrorCodeDto, String, Option<String>)> {
    events.iter().find_map(|event| match event {
        Ev::AppOperationFailed {
            code,
            message,
            request_id,
            ..
        } => Some((*code, message.clone(), request_id.clone())),
        _ => None,
    })
}

/// `CreateApp{mode: Shell}` lands an UNFORMED app: `scaffolded == false`,
/// a workspace holding only the private `.lingxi/` state dir plus the
/// GUIDED contract, and no application source at all.
///
/// ⚠️ The source check is a per-path absence, NOT an exact directory
/// listing: `git_enabled` defaults true and `layout.initialize()` may put
/// a `.git` in the workspace, so an equality assertion would fail for a
/// reason that has nothing to do with the property under test. The form
/// below still fails the moment a scaffold leaks in — which is what this
/// test is for.
#[test]
fn create_app_in_shell_mode_leaves_an_empty_workspace_with_the_guided_contract() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        // The "+" button's exact payload: empty name, empty brief, no
        // surface, `Shell`, and its own correlation key.
        let events = submit_create(
            &handle,
            &listener,
            "",
            "",
            None,
            AppCreateModeDto::Shell,
            Some("req-1"),
        )
        .await;
        let (record, request_id) =
            created_row(&events).expect("a Shell create must announce AppCreated");
        assert_eq!(
            request_id.as_deref(),
            Some("req-1"),
            "the client that pressed + recognises its own creation by this key"
        );
        assert!(
            !record.scaffolded,
            "a Shell create must land an UNFORMED record: {record:?}"
        );
        assert_eq!(
            record.name,
            local_apps::PLACEHOLDER_APP_NAME,
            "an empty wire name plus an empty brief derives the placeholder"
        );

        let workspace = tmp.path().join("apps").join(&record.id).join("workspace");
        assert!(
            workspace.join(".lingxi").is_dir(),
            "layout.initialize() must have run before the initializer"
        );
        assert!(
            workspace.join("LINGXI.md").is_file(),
            "the shell workspace must carry the guided contract"
        );
        for leaked in [
            "app",
            "src",
            "package.json",
            "vite.config.mjs",
            "index.html",
        ] {
            assert!(
                !workspace.join(leaked).exists(),
                "a shell workspace must hold no application source; found {leaked}"
            );
        }

        let contract =
            std::fs::read_to_string(workspace.join("LINGXI.md")).expect("guided contract");
        assert!(
            contract.contains("lingxi-local-app:create-local-app")
                && contract.contains("Immediately use the `Skill` tool"),
            "the thin shell must immediately enter the create coordinator: {contract}"
        );
        assert!(
            contract.contains(
                "Do not write source, build, install dependencies, or operate the runtime in \
                     this shell"
            ) && contract.contains("that refusal is the contract, not a transient failure"),
            "the contract must say a shapeless shell has nowhere for source to go, and that \
                 every tool refusing it is the contract rather than a transient failure: \
                 {contract}"
        );
        assert!(
            contract.contains("do not call `LocalAppScaffold` directly"),
            "the shell must preserve the direct-scaffold guard: {contract}"
        );
        assert!(
            contract.contains("Do not run a separate questionnaire")
                && contract
                    .contains("the Host prepares this workspace from the plan the user approves"),
            "the Host shell must leave the planning to the create skill and name that skill \
                 the owner of preparation: {contract}"
        );
        assert!(
            contract.contains(&record.id),
            "the contract must bind the workspace to its app id so the agent \
                 can enter the create flow without rediscovering it: {contract}"
        );
    });
}

/// A shape is decided when the scaffold lands, never before (§B.1), so a
/// `Shell` create that names a `surface` is REJECTED — it must not quietly
/// land a shell with the surface dropped on the floor.
#[test]
fn create_app_in_shell_mode_rejects_a_surface() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let events = submit_create(
            &handle,
            &listener,
            "",
            "",
            Some(client::protocol::local_apps::AppSurfaceDto::Dom),
            AppCreateModeDto::Shell,
            Some("req-2"),
        )
        .await;
        let (code, message, request_id) = first_failure(&events).unwrap_or_else(|| {
            panic!("a Shell create that names a surface must fail typed, got {events:?}")
        });
        assert_eq!(code, AppErrorCodeDto::InvalidRequest);
        assert!(
            message.contains("surface"),
            "the failure must name the offending field, got {message}"
        );
        assert_eq!(
            request_id.as_deref(),
            Some("req-2"),
            "even the pre-service rejection is the client's own failure"
        );
        assert!(
            created_row(&events).is_none(),
            "a rejected create must not land a record: {events:?}"
        );
    });
}

/// The record a `Shell` create commits lowers with `scaffolded == false`.
/// `lower_record` is a PURE mapping — proving it end-to-end here (real
/// service record in, DTO out) is what rules out a lowering that reads
/// the flag off something other than the record.
#[test]
fn a_shell_record_lowers_with_scaffolded_false() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let events = submit_create(
            &handle,
            &listener,
            "",
            "",
            None,
            AppCreateModeDto::Shell,
            None,
        )
        .await;
        let (row, _) = created_row(&events).expect("a Shell create must announce AppCreated");
        let record = handle
            .local_apps()
            .expect("local-apps service")
            .record(&row.id)
            .await
            .expect("the committed record");
        let dto = crate::mobile::local_apps_bridge::lower_record(tmp.path(), &record);
        assert!(
            !dto.scaffolded,
            "lower_record must carry the shell's own flag — no extra IO"
        );
    });
}

/// 🚨 `lower_record` is NOT `lower_app_event`: the test above pins the
/// record mapping and cannot stop the correlation key being dropped off
/// the EVENT. This one pins the event.
#[test]
fn lower_app_event_passes_request_id_through_on_app_created() {
    let record = local_apps::AppRecord {
        id: "app00001".into(),
        name: "Habits".into(),
        brief: "a habit tracker".into(),
        origin_cwd: None,
        mcp_intent: None,
        workflow_model: None,
        git_enabled: true,
        scaffolded: false,
        created_at_ms: 1,
        updated_at_ms: 2,
        conversation_id: None,
        init_session_id: None,
        workspace_rel: "apps/app00001/workspace".into(),
    };
    let root = tempfile::tempdir().expect("tempdir");
    let lowered = crate::mobile::local_apps_bridge::lower_app_event(
        root.path(),
        local_apps::AppEvent::AppCreated {
            record,
            request_id: Some("req-1".into()),
        },
    )
    .expect("AppCreated always has a wire representation");
    match lowered {
        Ev::AppEvent {
            event: AppEventDto::AppCreated { request_id, .. },
        } => {
            assert_eq!(
                request_id.as_deref(),
                Some("req-1"),
                "a dropped correlation key here means the + button never opens the new app"
            );
        }
        other => panic!("expected AppCreated, got {other:?}"),
    }
}

/// 🚨 A FAILED create must carry the client's own `request_id`.
///
/// `ClientEvent::AppOperationFailed.request_id` had no producer at all
/// before this handler threaded one through: the field existed and was
/// unconditionally `None`. A client that cannot claim its own failure
/// waits out a 30-second timeout and shows "创建结果未知" instead of the
/// real reason.
///
/// The failure is raised INSIDE `AppService` (an over-long name trips
/// `ensure_within`), so this also proves the key survives the deep path,
/// not just the handler's own early returns.
#[test]
fn a_failed_shell_create_reports_the_request_id_the_client_sent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let too_long = "n".repeat(local_apps::service::MAX_NAME_BYTES + 1);
        let events = submit_create(
            &handle,
            &listener,
            &too_long,
            "",
            None,
            AppCreateModeDto::Shell,
            Some("req-9"),
        )
        .await;
        let (code, message, request_id) = first_failure(&events)
            .unwrap_or_else(|| panic!("AppOperationFailed must be emitted, got {events:?}"));
        assert_eq!(code, AppErrorCodeDto::InvalidRequest, "{message}");
        assert_eq!(
            request_id.as_deref(),
            Some("req-9"),
            "否则客户端只能靠超时兜底"
        );
    });
}

/// 🚨 `Scaffolded` is no longer a reachable success path on the client
/// wire: v9 create is shell-only, and any stale caller must get a typed
/// failure carrying its own correlation key.
#[test]
fn a_scaffolded_create_reports_the_request_id_on_app_operation_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let events = submit_create(
            &handle,
            &listener,
            "Tracker",
            "a habit tracker",
            None,
            AppCreateModeDto::Scaffolded,
            Some("req-3"),
        )
        .await;
        let (code, message, request_id) = first_failure(&events).unwrap_or_else(|| {
            panic!("a Scaffolded create must fail typed on the v9 shell-only wire, got {events:?}")
        });
        assert_eq!(code, AppErrorCodeDto::InvalidRequest, "{message}");
        assert!(
            message.contains("shell"),
            "the failure must explain that CreateApp is shell-only now, got {message}"
        );
        assert_eq!(
            request_id.as_deref(),
            Some("req-3"),
            "the stale caller must still receive its own correlation key"
        );
        assert!(
            created_row(&events).is_none(),
            "a rejected Scaffolded create must not land a record: {events:?}"
        );
    });
}

/// 🚨 The wire still carries both enum variants, but only `Shell` is a
/// valid client create. Pinning success vs typed rejection keeps the
/// compatibility story explicit.
#[test]
fn create_app_accepts_shell_and_rejects_scaffolded_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (handle, listener) = build_submit_handle(tmp.path());

    handle.runtime().block_on(async {
        let shell = submit_create(
            &handle,
            &listener,
            "",
            "",
            None,
            AppCreateModeDto::Shell,
            None,
        )
        .await;
        let (shell_record, _) = created_row(&shell).expect("Shell create announces AppCreated");
        let shell_workspace = tmp
            .path()
            .join("apps")
            .join(&shell_record.id)
            .join("workspace");

        let scaffolded = submit_create(
            &handle,
            &listener,
            "Tracker",
            "a habit tracker",
            None,
            AppCreateModeDto::Scaffolded,
            None,
        )
        .await;
        let (code, message, _) = first_failure(&scaffolded).unwrap_or_else(|| {
            panic!("Scaffolded create must fail typed on the shell-only wire, got {scaffolded:?}")
        });

        assert!(
            !shell_record.scaffolded,
            "Shell lands unformed: {shell_record:?}"
        );
        assert!(
            !shell_workspace.join("package.json").exists(),
            "the Shell branch must lay down no scaffold"
        );
        assert_eq!(code, AppErrorCodeDto::InvalidRequest, "{message}");
        assert!(
            message.contains("shell"),
            "the rejection must explain the shell-only contract, got {message}"
        );
    });
}
