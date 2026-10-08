use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::host::{PromptSnapshot, PromptToolDescription};
use serde_json::json;
use session::jsonl::JsonlMessage;
use std::sync::{Arc, Mutex as StdMutex};
use tool_api::registry::ToolRegistry;

static ENV_LOCK: StdMutex<()> = StdMutex::new(());

fn assert_recorded_host_branding_role(request: &crate::OrchestratorApiRequest) {
    use lingxi_llm_client::protocol::ProtocolFamily;
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        project_system_prompt, PromptText, SystemPromptInput,
    };

    let crate::OrchestratorApiRequest::Main(request) = request else {
        panic!("the Host prompt is carried by a main request");
    };
    let system = request.system.as_ref().expect("system prompt");
    let SystemPromptInput::SourceVector {
        elements,
        host_branding_identity: Some(identity),
        ..
    } = system
    else {
        panic!("the Host source vector must carry its explicit branding identity");
    };
    let expected = PromptText::from_string(crate::prompt::locked_templates::HEADER);
    assert_eq!(identity.utf16_code_units(), expected.utf16_code_units());
    assert!(elements
        .iter()
        .any(|element| element.utf16_code_units() == expected.utf16_code_units()));

    // Feed the exact Host request object through the SDK's Anthropic projector.
    let blocks = project_system_prompt(
        system,
        None,
        &request.model,
        ProtocolFamily::AnthropicMessages,
        Default::default(),
    );
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[0].block.text,
        crate::prompt::locked_templates::HEADER
    );
}

fn attachment(payload: serde_json::Value) -> JsonlMessage {
    JsonlMessage { json_projection: None,
        message_type: "attachment".to_string(),
        uuid: "11111111-2222-4333-8444-555555555555".to_string(),
        parent_uuid: None,
        session_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string(),
        timestamp: "2026-08-30T00:00:00.000Z".to_string(),
        cwd: "/tmp/project".to_string(),
        version: "0.12.0".to_string(),
        message: serde_json::Value::Null,
        is_sidechain: false,
        user_type: Some("external".to_string()),
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra: [("attachment".to_string(), payload)].into_iter().collect(),
    }
}

#[test]
fn prompt_snapshot_serializes_to_oracle_payload_shape() {
    let snapshot = PromptSnapshot {
        context_rendering: Some(lingxi_core::host::instructions::InstructionRendering::Announced),
        system_prompt: vec!["frozen static prompt".to_string()],
        system_prompt_utf16: Vec::new(),
        tools: vec![PromptToolDescription {
            name: "Read".to_string(),
            description: "old description".to_string(),
        }],
    };
    let value = serde_json::to_value(snapshot).expect("snapshot serializes");
    assert_eq!(value["systemPrompt"], json!(["frozen static prompt"]));
    assert_eq!(value["contextRendering"], "announced");
    assert_eq!(value["tools"][0]["name"], "Read");
    assert_eq!(value["tools"][0]["description"], "old description");
    assert!(
        value.get("type").is_none(),
        "attachment type belongs to the envelope"
    );
}

#[test]
fn prompt_snapshot_reads_only_current_native_fields() {
    let snapshot: PromptSnapshot = serde_json::from_value(json!({
        "systemPrompt": ["current"],
        "contextRendering": "announced",
        "tools": [{"name": "Read", "description": "current"}],
        "context_rendering": "inline",
        "recordedToolDescriptions": [{"name": "Old", "description": "old"}],
        "toolDescriptions": [{"name": "OldAlias", "description": "old"}],
    }))
    .expect("unknown non-Native fields are stripped");
    assert_eq!(snapshot.system_prompt, ["current"]);
    assert_eq!(
        snapshot.context_rendering,
        Some(lingxi_core::host::instructions::InstructionRendering::Announced)
    );
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools[0].name, "Read");
    let serialized = serde_json::to_value(snapshot).expect("canonical snapshot serializes");
    assert!(serialized.get("context_rendering").is_none());
    assert!(serialized.get("recordedToolDescriptions").is_none());
    assert!(serialized.get("toolDescriptions").is_none());
}

#[tokio::test]
async fn snapshot_persists_the_native_marker_as_its_own_source_block() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var(branding::SIMPLE_ENV);
    lingxi_core::host::session_flags::set_system_prompt_snapshot(None);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let marker = lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY;
    *orch
        .prompt_runtime
        .pending_prompt_source_vector
        .lock()
        .await = Some(vec!["shared".into(), marker.into(), "session".into()]);
    let provider_prompt =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "shared",
                ),
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    marker,
                ),
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "session",
                ),
            ],
            Some(
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "live context",
                ),
            ),
            None,
        );
    orch.record_prompt_snapshot_if_needed(Some(&provider_prompt), &[])
        .await;
    let snapshot = orch
        .prompt_runtime
        .prompt_snapshot
        .lock()
        .await
        .clone()
        .unwrap();
    assert_eq!(
        snapshot.system_prompt,
        vec!["shared".to_owned(), marker.to_owned(), "session".to_owned()]
    );
    let persisted = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        persisted["systemPrompt"],
        json!(["shared", marker, "session"])
    );
    assert!(persisted.get("sharedBoundary").is_none());
    let restored = crate::resume::prompt_snapshot_from_messages(&[attachment(json!({
        "type": "prompt_snapshot",
        "systemPrompt": snapshot.system_prompt,
    }))])
    .unwrap();
    assert_eq!(
        restored.system_prompt,
        vec!["shared".to_owned(), marker.to_owned(), "session".to_owned()]
    );
    let raw = orch.effective_system_prompt().await;
    let provider = orch.provider_system_prompt().await.display_text();
    assert!(raw.starts_with("shared\n\nsession"));
    assert!(provider.starts_with(&format!("shared\n\n{marker}\n\nsession")));
}

#[test]
fn prompt_snapshot_resume_uses_last_valid_attachment() {
    let valid = attachment(json!({
        "type": "prompt_snapshot",
        "systemPrompt": ["first static"],
        "tools": [{"name": "Read", "description": "v1"}],
    }));
    let invalid = attachment(json!({
        "type": "prompt_snapshot",
        "systemPrompt": [7],
    }));
    let snapshot = crate::resume::prompt_snapshot_from_messages(&[valid, invalid])
        .expect("the last valid snapshot should be recovered");
    assert_eq!(snapshot.system_prompt, vec!["first static"]);
    assert_eq!(snapshot.tools[0].name, "Read");
    assert_eq!(snapshot.tools[0].description, "v1");
}

#[tokio::test]
async fn prompt_snapshot_appends_only_new_inline_tools_after_success() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var(branding::SIMPLE_ENV);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    *orch.prompt_runtime.prompt_snapshot.lock().await = Some(PromptSnapshot {
        context_rendering: None,
        system_prompt: vec!["frozen".to_string()],
        system_prompt_utf16: Vec::new(),
        tools: vec![PromptToolDescription {
            name: "Read".to_string(),
            description: "frozen Read".to_string(),
        }],
    });
    orch.record_inline_prompt_tools_after_success(&[
        json!({"name": "Read", "description": "live Read"}),
        json!({"name": "Deferred", "description": "not inline", "defer_loading": true}),
        json!({"name": "Write", "description": "new Write"}),
    ])
    .await;
    let snapshot = orch
        .prompt_runtime
        .prompt_snapshot
        .lock()
        .await
        .clone()
        .unwrap();
    assert_eq!(
        snapshot
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.description.as_str()))
            .collect::<Vec<_>>(),
        vec![("Read", "frozen Read"), ("Write", "new Write")]
    );
}

#[tokio::test]
async fn resumed_session_without_snapshot_never_creates_one() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var(branding::SIMPLE_ENV);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.prompt_runtime
        .prompt_snapshot_resume
        .store(true, std::sync::atomic::Ordering::Release);
    let rendered = orch.provider_system_prompt().await;
    orch.record_prompt_snapshot_if_needed(Some(&rendered), &[])
        .await;
    assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
}

/// Oracle `lje(e)`'s truth table. `explicit` is the `--system-prompt-snapshot`
/// choice, which is THREE-valued: absent defers to the disjuncts, `off`
/// overrules all of them, `on` forces the feature regardless of session kind.
#[test]
fn the_snapshot_gate_matches_the_oracle_truth_table() {
    use ConversationOrchestrator as O;
    // `if (e.systemPromptSnapshot === false) return false` comes FIRST, so an
    // explicit `off` beats every disjunct that would otherwise enable it.
    assert!(!O::snapshot_gate(Some(false), true, false));
    assert!(!O::snapshot_gate(Some(false), false, false));
    assert!(!O::snapshot_gate(Some(false), true, true));

    // An explicit `on` forces it even in simple mode.
    assert!(O::snapshot_gate(Some(true), false, true));

    // Absent: `SESSION_KIND === "bg" || !LINGXI_SIMPLE`.
    assert!(O::snapshot_gate(None, true, true), "bg wins over simple");
    assert!(
        !O::snapshot_gate(None, false, true),
        "simple alone disables"
    );
    assert!(
        O::snapshot_gate(None, false, false),
        "the DEFAULT is on: 2.1.270 has no rollout flag left to wait for"
    );
}

/// The regression this fixes: with no env set and no flag passed, the snapshot
/// must be ELIGIBLE. The port used to require `tengu_carved_slate` /
/// `CLAUDE_CODE_CARVED_SLATE`, a 2.1.252 rollout flag that no longer exists
/// anywhere in the 2.1.270 binary — so the whole recorded-prompt feature was
/// implemented, tested, restored on resume, and unreachable.
#[tokio::test]
async fn a_plain_session_records_its_prompt_without_any_env_opt_in() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var(branding::SIMPLE_ENV);
    std::env::remove_var("CLAUDE_CODE_CARVED_SLATE");
    std::env::remove_var("LINGXI_SESSION_KIND");
    lingxi_core::host::session_flags::set_system_prompt_snapshot(None);

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    assert!(
        orch.prompt_snapshot_eligible(),
        "no env, no flag — 2.1.270 records by default"
    );
    let rendered = orch.provider_system_prompt().await;
    orch.record_prompt_snapshot_if_needed(Some(&rendered), &[])
        .await;
    assert!(
        orch.prompt_runtime.prompt_snapshot.lock().await.is_some(),
        "an eligible session must actually record, not merely qualify"
    );
}

/// `--system-prompt-snapshot off` is the escape hatch the flag exists for:
/// "never record; the prompt is rendered fresh every request (for iterating on
/// prompt text)". It must beat the default that the test above pins.
#[tokio::test]
async fn the_off_flag_stops_recording_a_session_that_would_otherwise_record() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var(branding::SIMPLE_ENV);
    std::env::remove_var("LINGXI_SESSION_KIND");
    lingxi_core::host::session_flags::set_system_prompt_snapshot(Some(false));

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    assert!(!orch.prompt_snapshot_eligible());
    let rendered = orch.provider_system_prompt().await;
    orch.record_prompt_snapshot_if_needed(Some(&rendered), &[])
        .await;
    assert!(
        orch.prompt_runtime.prompt_snapshot.lock().await.is_none(),
        "`off` must mean no record at all"
    );
    lingxi_core::host::session_flags::set_system_prompt_snapshot(None);
}

#[tokio::test]
async fn prompt_snapshot_writer_rendering_survives_disk_cold_resume_and_actual_request() {
    use lingxi_core::host::instructions::InstructionRendering;
    use lingxi_core::host::FileSystem;
    use platform_posix::fs::PosixFileSystem;
    use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};

    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    struct RestoreSnapshotChoice(Option<bool>);
    impl Drop for RestoreSnapshotChoice {
        fn drop(&mut self) {
            lingxi_core::host::session_flags::set_system_prompt_snapshot(self.0);
            telemetry::test_clear_flag("tengu_foamy_spring");
        }
    }
    let _restore =
        RestoreSnapshotChoice(lingxi_core::host::session_flags::system_prompt_snapshot());
    lingxi_core::host::session_flags::set_system_prompt_snapshot(Some(true));
    telemetry::test_set_flag("tengu_foamy_spring", true);

    let response = || {
        crate::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "answer".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )
    };
    for rendering in [
        InstructionRendering::Inline,
        InstructionRendering::Announced,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let cwd_string = cwd.to_string_lossy().into_owned();
        let home = dir.path().join("home");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_owned()));
        let warm_api = Arc::new(MockApiClient::new(vec![response()]));
        let warm = ConversationOrchestrator::new(
            OrchestratorConfig {
                context_rendering: rendering,
                ..OrchestratorConfig::default()
            },
            warm_api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_config_home(home.clone());
        let session_id = warm.session.lock().await.session_id;
        let path =
            session::jsonl::session_path(&home, &cwd_string, &session_id.as_uuid().to_string());
        let warm = warm.with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone())));
        warm.run_turn("warm prompt").await.unwrap();
        let warm_requests = warm_api.captured_requests().await;
        assert_eq!(warm_requests.len(), 1);
        assert_recorded_host_branding_role(&warm_requests[0]);
        // Exercise the same writer a second time through the successful inline
        // tool update path; both persisted versions must retain the hint.
        warm.record_inline_prompt_tools_after_success(&[json!({
            "name":"LateInline", "description":"new inline description"
        })])
        .await;
        let expected_snapshot = warm
            .prompt_runtime
            .prompt_snapshot
            .lock()
            .await
            .clone()
            .unwrap();
        assert_eq!(expected_snapshot.context_rendering, Some(rendering));
        drop(warm);

        let rows = JsonlReader::new(path, fs.clone()).read_all().await.unwrap();
        let snapshots = rows
            .iter()
            .filter_map(|row| row.extra.get("attachment"))
            .filter(|payload| payload["type"] == "prompt_snapshot")
            .collect::<Vec<_>>();
        assert_eq!(snapshots.len(), 2);
        for payload in snapshots {
            assert_eq!(payload["contextRendering"], json!(rendering));
            assert!(payload.get("hostBrandingIdentity").is_none());
            assert!(payload.get("brandingIdentity").is_none());
        }

        let cold_marker = "policy loaded only after cold resume";
        let cold_api = Arc::new(MockApiClient::new(vec![response()]));
        let cold = ConversationOrchestrator::with_resume(
            OrchestratorConfig::default(),
            session_id.as_uuid(),
            home,
            cwd_string,
            fs,
            cold_api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::with_files(vec![
                crate::prompt::MemoryFile {
                    source_content: None,
                    parent: None,
                    path: cwd.join(branding::MEMORY_FILE),
                    body: cold_marker.into(),
                    raw_content: cold_marker.into(),
                    is_local_override: false,
                    tier: memory::lingxi_md::LingxiMdTier::Project,
                    globs: None,
                    content_differs_from_disk: false,
                },
            ])),
            cwd,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            *cold.prompt_runtime.context_rendering_hint.lock().await,
            Some(rendering),
            "the disk loader must recover the real writer's hint"
        );
        let cold_snapshot = cold
            .prompt_runtime
            .prompt_snapshot
            .lock()
            .await
            .clone()
            .expect("the Native prompt snapshot is restored");
        assert_eq!(cold_snapshot.system_prompt, expected_snapshot.system_prompt);
        assert_eq!(cold_snapshot.tools, expected_snapshot.tools);
        assert_eq!(
            cold_snapshot.context_rendering,
            expected_snapshot.context_rendering
        );
        assert!(expected_snapshot
            .system_prompt_utf16
            .iter()
            .all(Option::is_none));
        assert!(
            cold_snapshot.system_prompt_utf16.is_empty(),
            "the Native snapshot attachment has no parallel UTF-16 sidecar field"
        );
        assert_eq!(cold.current_context_rendering().await, rendering);
        cold.run_turn("cold prompt").await.unwrap();
        let cold_requests = cold_api.captured_requests().await;
        assert_eq!(cold_requests.len(), 1);
        assert_recorded_host_branding_role(&cold_requests[0]);
        let captured = cold_api.captured_msgs().await;
        assert_eq!(captured.len(), 1);
        let policy_messages = captured[0]
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::User { content, .. } => Some((
                    message.id(),
                    content
                        .iter()
                        .filter_map(lingxi_core::types::ContentBlock::visible_text)
                        .collect::<Vec<_>>()
                        .join("\n"),
                )),
                _ => None,
            })
            .filter(|(_, text)| text.contains(cold_marker))
            .collect::<Vec<_>>();
        assert_eq!(policy_messages.len(), 1);
        match rendering {
            InstructionRendering::Inline => assert!(policy_messages[0].1.contains(
                "As you answer the user's questions, you can use the following context:"
            )),
            InstructionRendering::Announced => {
                let raw_attachment = cold
                    .transcript
                    .model_reminder_attachments
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&policy_messages[0].0)
                    .cloned()
                    .expect("announced policy retains its typed attachment");
                assert_eq!(raw_attachment["type"], "instructions");
                assert_eq!(
                    lingxi_core::host::instruction_announcements::render_instruction_attachment(
                        &raw_attachment,
                    ),
                    Some(policy_messages[0].1.clone())
                );
                assert!(!policy_messages[0]
                    .1
                    .contains("As you answer the user's questions"));
            }
        }
    }
}

#[tokio::test]
async fn prompt_snapshot_writer_omits_absent_rendering_on_legacy_tool_update() {
    use lingxi_core::host::FileSystem;
    use platform_posix::fs::PosixFileSystem;
    use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};

    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    struct RestoreSnapshotChoice(Option<bool>);
    impl Drop for RestoreSnapshotChoice {
        fn drop(&mut self) {
            lingxi_core::host::session_flags::set_system_prompt_snapshot(self.0);
        }
    }
    let _restore =
        RestoreSnapshotChoice(lingxi_core::host::session_flags::system_prompt_snapshot());
    lingxi_core::host::session_flags::set_system_prompt_snapshot(Some(true));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_owned()));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_owned(),
    )
    .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone())));
    *orch.prompt_runtime.prompt_snapshot.lock().await = Some(PromptSnapshot {
        system_prompt: vec!["legacy static prefix".into()],
        system_prompt_utf16: Vec::new(),
        tools: Vec::new(),
        context_rendering: None,
    });
    orch.record_inline_prompt_tools_after_success(&[json!({
        "name":"Read", "description":"new inline description"
    })])
    .await;
    let rows = JsonlReader::new(path, fs).read_all().await.unwrap();
    let payload = rows
        .iter()
        .filter_map(|row| row.extra.get("attachment"))
        .find(|payload| payload["type"] == "prompt_snapshot")
        .unwrap();
    assert!(payload.get("contextRendering").is_none());
    assert_eq!(payload["systemPrompt"], json!(["legacy static prefix"]));
    assert_eq!(payload["tools"][0]["name"], "Read");
}

#[tokio::test]
async fn provider_prompt_keeps_live_git_context_after_app_agent_profile() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    struct RestoreSnapshotChoice(Option<bool>);
    impl Drop for RestoreSnapshotChoice {
        fn drop(&mut self) {
            lingxi_core::host::session_flags::set_system_prompt_snapshot(self.0);
        }
    }
    let _restore =
        RestoreSnapshotChoice(lingxi_core::host::session_flags::system_prompt_snapshot());
    std::env::remove_var(branding::SIMPLE_ENV);
    std::env::remove_var("LINGXI_SESSION_KIND");
    lingxi_core::host::session_flags::set_system_prompt_snapshot(None);

    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .expect("git is available")
            .success()
    };
    assert!(git(&["init", "-q"]));
    assert!(git(&["config", "user.name", "Prompt Snapshot Test"]));
    assert!(git(&[
        "config",
        "user.email",
        "prompt-snapshot@example.invalid"
    ]));
    assert!(git(&["config", "commit.gpgsign", "false"]));
    std::fs::write(repo.path().join("tracked.txt"), "before").unwrap();
    assert!(git(&["add", "tracked.txt"]));
    assert!(git(&["commit", "-q", "-m", "seed"]));
    std::fs::write(repo.path().join("tracked.txt"), "live git context marker").unwrap();

    let cwd = repo.path().to_path_buf();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            context_rendering: lingxi_core::host::instructions::InstructionRendering::Inline,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.clone(),
    )
    .with_session_cwd(tool_api::SessionCwd::new(cwd, Vec::new()));
    orch.set_app_agent_prompt_profile(1, "APP_PROFILE_MARKER".to_owned())
        .unwrap();

    let provider_prompt = orch.provider_system_prompt().await.display_text();
    let profile_at = provider_prompt
        .find("APP_PROFILE_MARKER")
        .expect("app profile remains in the provider prompt");
    let git_at = provider_prompt
        .find("gitStatus: This is the git status at the start of the conversation.")
        .expect("live git context remains in the provider prompt");
    assert!(
        profile_at < git_at,
        "dynamic systemContext must follow the full static source vector"
    );
    assert!(provider_prompt.contains("M tracked.txt"));
}

#[tokio::test]
async fn prompt_snapshot_keeps_raw_utf16_on_disk_but_cold_domain_uses_native_display() {
    use lingxi_core::host::FileSystem;
    use platform_posix::fs::PosixFileSystem;
    use session::jsonl::{exact_json, reader::JsonlReader, writer::JsonlWriter};

    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    struct RestoreSnapshotChoice(Option<bool>);
    impl Drop for RestoreSnapshotChoice {
        fn drop(&mut self) {
            lingxi_core::host::session_flags::set_system_prompt_snapshot(self.0);
            telemetry::test_clear_flag("tengu_foamy_spring");
        }
    }
    let _restore =
        RestoreSnapshotChoice(lingxi_core::host::session_flags::system_prompt_snapshot());
    lingxi_core::host::session_flags::set_system_prompt_snapshot(Some(true));
    telemetry::test_set_flag("tengu_foamy_spring", true);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prompt-utf16.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_owned()));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_owned(),
    )
    .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone())));
    let units = vec![0xd800, 0x03a9, 0xdc00];
    let prompt_text =
        lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
            units.clone(),
        );
    *orch
        .prompt_runtime
        .pending_prompt_source_vector
        .lock()
        .await = Some(vec![prompt_text.clone()]);
    let input =
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![prompt_text],
            None,
            None,
        );
    orch.record_prompt_snapshot_if_needed(Some(&input), &[])
        .await;

    let raw = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        raw.contains("\\ud800"),
        "raw JSONL must retain the lone high surrogate"
    );
    assert!(
        raw.contains("\\udc00"),
        "raw JSONL must retain the lone low surrogate"
    );
    assert!(
        !raw.contains("lingxi_exact_json_utf16"),
        "private data must not leak on disk"
    );

    let rows = JsonlReader::new(path, fs).read_all().await.unwrap();
    let snapshot_row = rows
        .iter()
        .find(|row| {
            row.extra
                .get("attachment")
                .is_some_and(|payload| payload["type"] == "prompt_snapshot")
        })
        .unwrap();
    let attachment = &snapshot_row.extra["attachment"];
    assert_eq!(attachment["systemPrompt"], json!(["�Ω�"]));
    assert_eq!(
        exact_json::message_utf16_overrides(snapshot_row)["/attachment/systemPrompt/0"],
        units
    );
    let cold = crate::resume::prompt_snapshot_from_messages(&rows).unwrap();
    assert_eq!(cold.system_prompt, vec!["�Ω�"]);
    assert!(cold.system_prompt_utf16.is_empty());
}
