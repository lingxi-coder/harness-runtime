use super::restore_session_worktree_state;
use lingxi_core::host::FileSystem;
use std::sync::Arc;

struct ScheduledAnchorPermissionSink;

fn scheduled_test_writer(
    cwd: &std::path::Path,
    path: &std::path::Path,
    fs: Arc<dyn FileSystem>,
    id: uuid::Uuid,
) -> Arc<session::JsonlWriter> {
    let state = cwd.join("test-session-state").join(id.to_string());
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let authority = Arc::new(session::jsonl::DurableTranscriptWriter::open(state).unwrap());
    let writer =
        Arc::new(session::JsonlWriter::new(path.to_path_buf(), fs).with_durable_lock(authority));
    writer
        .activate_session_target(
            lingxi_core::types::SessionId::from_uuid(id),
            path.to_path_buf(),
            cwd.to_path_buf(),
        )
        .unwrap();
    writer
}

#[async_trait::async_trait]
impl client::adapter::PermissionRequestSink for ScheduledAnchorPermissionSink {
    async fn emit_request(&self, _: client::protocol::permission::PermissionRequest) {}
}

#[tokio::test]
async fn scheduled_draft_is_discoverable_before_desktop_build_returns() {
    use lingxi_core::host::session_context::ConversationMode;
    let (_root, mut cfg) = super::tests::test_config(true);
    let id = uuid::Uuid::new_v4();
    cfg.session_id_override = Some(id.to_string());
    cfg.initial_conversation_mode = Some(ConversationMode::Chat);
    cfg.restore_draft = true;
    cfg.durable_draft_title = Some("Scheduled task".into());
    cfg.enable_automation_scheduler = false;
    cfg.isolated_credential_storage = true;
    let path = session::jsonl::session_path(
        &cfg.lingxi_home,
        &cfg.cwd.to_string_lossy(),
        &id.to_string(),
    );
    let fs: Arc<dyn FileSystem> =
        Arc::new(super::platform::DesktopFileSystem::new(cfg.cwd.clone()));
    let runtime = Box::pin(super::build(
        cfg.clone(),
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(ScheduledAnchorPermissionSink),
    ))
    .await
    .unwrap();
    let source = std::fs::read_to_string(&path).unwrap();
    let rows = source
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["type"], "fixed-session-mode");
    assert!(rows.iter().any(|row| row["type"] == "custom-title"
        && row["mobileEmptySession"] == 1
        && row["sessionId"] == id.to_string()
        && row["cwd"].as_str() == cfg.cwd.to_str()));
    let catalog = session::jsonl::loader::list_recent_sessions(
        &cfg.lingxi_home,
        &cfg.cwd.to_string_lossy(),
        20,
        fs,
    )
    .await
    .unwrap();
    assert!(catalog
        .iter()
        .any(|row| row.uuid == id && row.mode == ConversationMode::Chat && row.message_count == 0));
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
    drop(runtime);

    let captured = std::fs::read_to_string(&path).unwrap();
    cfg.lingxi_home = cfg.lingxi_home.canonicalize().unwrap();
    let physical_path = path.canonicalize().unwrap();
    let (authority, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
        cfg.lingxi_home.clone(),
        physical_path
            .strip_prefix(&cfg.lingxi_home)
            .unwrap()
            .to_path_buf(),
    )
    .unwrap();
    let retry = super::DesktopConfig {
        session_transcript_path: Some(authority.path()),
        session_transcript_authority: Some(authority),
        session_resume_snapshot: Some(super::DesktopSessionResumeSnapshot {
            source_session_id: id.to_string(),
            content: captured.into(),
        }),
        ..cfg
    };
    let resumed = Box::pin(super::build(
        retry,
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(ScheduledAnchorPermissionSink),
    ))
    .await
    .unwrap();
    let resumed_source = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        resumed_source
            .lines()
            .filter(|line| serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|row| row["mobileEmptySession"] == 1))
            .count(),
        1
    );
    assert!(
        resumed
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}

#[tokio::test]
async fn scheduled_anchor_is_idempotent_and_rejects_foreign_owner() {
    use lingxi_core::host::session_context::ConversationMode;
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let home = cwd.join("home");
    let id = uuid::Uuid::new_v4();
    let path = session::jsonl::session_path(&home, &cwd.to_string_lossy(), &id.to_string());
    let fs: Arc<dyn FileSystem> = Arc::new(super::platform::DesktopFileSystem::new(cwd.clone()));
    let writer = scheduled_test_writer(&cwd, &path, fs, id);
    let _lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
        .claim_session_id(&id.to_string(), std::process::id())
        .unwrap();
    writer
        .initialize_fixed_session_mode(&id.to_string(), ConversationMode::Chat, 0)
        .await
        .unwrap();
    assert!(super::assembly::ensure_durable_draft_anchor(
        &writer,
        &path,
        &cwd,
        &id.to_string(),
        ConversationMode::Chat,
        "Scheduled task"
    )
    .await
    .unwrap());
    let anchored = std::fs::read(&path).unwrap();
    assert!(!super::assembly::ensure_durable_draft_anchor(
        &writer,
        &path,
        &cwd,
        &id.to_string(),
        ConversationMode::Chat,
        "Scheduled task"
    )
    .await
    .unwrap());
    assert!(super::assembly::ensure_durable_draft_anchor(
        &writer,
        &path,
        &cwd,
        &id.to_string(),
        ConversationMode::Code,
        "Scheduled task"
    )
    .await
    .is_err());
    assert!(super::assembly::ensure_durable_draft_anchor(
        &writer,
        &path,
        &cwd.join("other"),
        &id.to_string(),
        ConversationMode::Chat,
        "Scheduled task"
    )
    .await
    .is_err());
    assert!(super::assembly::ensure_durable_draft_anchor(
        &writer,
        &path,
        &cwd,
        &uuid::Uuid::new_v4().to_string(),
        ConversationMode::Chat,
        "Scheduled task"
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), anchored);
    let relocated =
        serde_json::json!({"type":"relocated", "sessionId":id, "relocatedCwd":cwd.join("other")});
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{relocated}"
    )
    .unwrap();
    let foreign_relocation = std::fs::read(&path).unwrap();
    assert!(super::assembly::ensure_durable_draft_anchor(
        &writer,
        &path,
        &cwd,
        &id.to_string(),
        ConversationMode::Chat,
        "Scheduled task"
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), foreign_relocation);
    std::fs::write(&path, &anchored).unwrap();

    let existing_id = uuid::Uuid::new_v4();
    let existing_path =
        session::jsonl::session_path(&home, &cwd.to_string_lossy(), &existing_id.to_string());
    let fs: Arc<dyn FileSystem> = Arc::new(super::platform::DesktopFileSystem::new(cwd.clone()));
    let existing = scheduled_test_writer(&cwd, &existing_path, fs, existing_id);
    let _existing_lease =
        lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
            .claim_session_id(&existing_id.to_string(), std::process::id())
            .unwrap();
    existing
        .initialize_fixed_session_mode(&existing_id.to_string(), ConversationMode::Chat, 0)
        .await
        .unwrap();
    let row = serde_json::json!({"type":"user","uuid":uuid::Uuid::new_v4(),"sessionId":existing_id,"parentUuid":null,"cwd":cwd,"message":{"role":"user","content":"existing history"}});
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&existing_path)
            .unwrap(),
        "{row}"
    )
    .unwrap();
    let original = std::fs::read(&existing_path).unwrap();
    assert!(
        !session::jsonl::route_lines(std::str::from_utf8(&original).unwrap())
            .messages_in_order
            .is_empty()
    );
    assert!(!super::assembly::ensure_durable_draft_anchor(
        &existing,
        &existing_path,
        &cwd,
        &existing_id.to_string(),
        ConversationMode::Chat,
        "Scheduled task"
    )
    .await
    .unwrap());
    assert_eq!(std::fs::read(&existing_path).unwrap(), original);
}

struct ColdResumeTool(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl tool_api::Tool for ColdResumeTool {
    fn name(&self) -> &str {
        "NeedsApproval"
    }
    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<lingxi_core::types::utf16_json::Utf16JsonProjection> =
            std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"type":"object","properties":{}}),
            )
            .unwrap()
        })
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        false
    }
    async fn check_permissions(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "cold resume fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        "needs approval".into()
    }
    async fn prompt(
        &self,
        _: &tool_api::tool_trait::PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        "".into()
    }
    async fn call(
        &self,
        _: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(tool_api::ToolCallResult::from_data("called".into()))
    }
}

struct ColdResumePermissionGate(Arc<std::sync::atomic::AtomicUsize>);

#[test]
fn mode_only_draft_requires_exactly_one_valid_fixed_header() {
    let sid = uuid::Uuid::new_v4().to_string();
    let fixed = serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"chat", "instructionEpoch":3});
    let source = format!("{fixed}\n");
    assert!(super::session_state::is_mode_only_draft(&source, &sid));
    assert!(!super::session_state::is_mode_only_draft(
        source.trim_end(),
        &sid
    ));
    for bad in [
        serde_json::json!({"type":"session-mode", "sessionId":sid, "sessionMode":"chat"}),
        serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"chat"}),
        serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"bogus", "instructionEpoch":0}),
        serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"chat", "instructionEpoch":0, "pending":false}),
    ] {
        assert!(!super::session_state::is_mode_only_draft(
            &format!("{bad}\n"),
            &sid
        ));
        assert!(!super::session_state::is_mode_only_draft(
            &format!("{source}{bad}\n"),
            &sid
        ));
    }
}

#[test]
fn draft_lifecycle_attachments_require_the_host_shape_and_a_closed_chain() {
    let sid = uuid::Uuid::new_v4();
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let hook_id = uuid::Uuid::new_v4();
    let row = |id, parent: serde_json::Value, attachment| {
        serde_json::json!({
            "type":"attachment", "sessionId":sid, "uuid":id, "parentUuid":parent,
            "isSidechain":false, "cwd":"/work", "version":"1", "timestamp":"2026-10-28T00:00:00Z",
            "attachment":attachment
        })
    };
    let startup = row(
        first,
        serde_json::Value::Null,
        serde_json::json!({
            "type":"hook_success", "hookName":"SessionStart:startup", "hookEvent":"SessionStart",
            "toolUseID":hook_id, "content":"", "stdout":"ok", "stderr":"", "exitCode":0,
            "command":"startup", "durationMs":1
        }),
    );
    let context = row(
        second,
        serde_json::json!(first),
        serde_json::json!({
            "type":"hook_additional_context", "hookName":"SessionStart", "hookEvent":"SessionStart",
            "toolUseID":"SessionStart", "content":["hook context"]
        }),
    );
    let mode = serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"chat", "instructionEpoch":1});
    let source = format!("{mode}\n{startup}\n{context}\n");
    assert!(super::session_state::is_mode_only_draft(
        &source,
        &sid.to_string()
    ));
    assert!(!super::session_state::is_mode_only_draft(
        source.trim_end_matches('\n'),
        &sid.to_string()
    ));
    let mut wrong_parent = context.clone();
    wrong_parent["parentUuid"] = serde_json::json!(uuid::Uuid::new_v4());
    let mut foreign = context.clone();
    foreign["sessionId"] = serde_json::json!(uuid::Uuid::new_v4());
    let mut duplicate = context.clone();
    duplicate["uuid"] = serde_json::json!(first);
    let mut tool = startup.clone();
    tool["attachment"]["type"] = serde_json::json!("hook_deferred_tool");
    let mut prompt = startup.clone();
    prompt["attachment"]["hookEvent"] = serde_json::json!("UserPromptSubmit");
    prompt["attachment"]["hookName"] = serde_json::json!("UserPromptSubmit");
    let mut bad_context = context.clone();
    bad_context["attachment"]["content"] = serde_json::json!([{"type":"tool_use"}]);
    let mut hidden_message = context.clone();
    hidden_message["message"] = serde_json::json!({"role":"user", "content":"not a draft"});
    let mut rendered = context.clone();
    rendered["renderedRole"] = serde_json::json!("user");
    let mut sidechain = context.clone();
    sidechain["isSidechain"] = serde_json::json!(true);
    let mut missing_uuid = context.clone();
    missing_uuid["uuid"] = serde_json::json!("not-a-uuid");
    let mut incomplete_success = startup.clone();
    incomplete_success["uuid"] = serde_json::json!(uuid::Uuid::new_v4());
    incomplete_success["parentUuid"] = serde_json::json!(first);
    incomplete_success["attachment"]["exitCode"] = serde_json::Value::Null;
    for bad in [
        wrong_parent,
        foreign,
        duplicate,
        tool,
        prompt,
        bad_context,
        hidden_message,
        rendered,
        sidechain,
        missing_uuid,
        incomplete_success,
    ] {
        assert!(
            !super::session_state::is_mode_only_draft(
                &format!("{mode}\n{startup}\n{bad}\n"),
                &sid.to_string()
            ),
            "accepted {bad}"
        );
    }
    for kind in ["user", "assistant", "system"] {
        let bad = serde_json::json!({"type":kind, "sessionId":sid, "uuid":uuid::Uuid::new_v4()});
        assert!(!super::session_state::is_mode_only_draft(
            &format!("{source}{bad}\n"),
            &sid.to_string()
        ));
    }
    assert!(!super::session_state::is_mode_only_draft(
        &format!("{source}{{bad\n"),
        &sid.to_string()
    ));

    // These hooks can run on an idle draft during startup, a model change,
    // settings reload, or graceful shutdown without a user turn.
    for event in [
        "SessionEnd",
        "InstructionsLoaded",
        "PreModelSwitch",
        "PostModelSwitch",
        "ConfigChange",
        "FileChanged",
    ] {
        let lifecycle = row(
            uuid::Uuid::new_v4(),
            serde_json::Value::Null,
            serde_json::json!({
                "type":"hook_success", "hookName":event, "hookEvent":event, "toolUseID":uuid::Uuid::new_v4(),
                "content":"", "stdout":"", "stderr":"", "exitCode":0, "command":"hook", "durationMs":1
            }),
        );
        assert!(super::session_state::is_mode_only_draft(
            &format!("{mode}\n{lifecycle}\n"),
            &sid.to_string()
        ));
    }
    for payload in [
        serde_json::json!({"type":"hook_non_blocking_error", "hookName":"SessionStart:startup",
            "hookEvent":"SessionStart", "toolUseID":hook_id, "stderr":"failed", "stdout":"", "exitCode":1}),
        serde_json::json!({"type":"hook_cancelled", "hookName":"SessionStart:startup",
            "hookEvent":"SessionStart", "toolUseID":hook_id}),
        serde_json::json!({"type":"hook_system_message", "hookName":"SessionStart:startup",
            "hookEvent":"SessionStart", "toolUseID":hook_id, "content":"notice"}),
    ] {
        let attachment = row(uuid::Uuid::new_v4(), serde_json::Value::Null, payload);
        assert!(super::session_state::is_mode_only_draft(
            &format!("{mode}\n{attachment}\n"),
            &sid.to_string()
        ));
    }
}

#[test]
fn unprompted_code_draft_requires_fixed_header_before_typed_metadata() {
    let sid = uuid::Uuid::new_v4();
    let permission = serde_json::json!({"type":"permission-mode", "sessionId":sid,
        "permissionMode":"acceptEdits"});
    let title = serde_json::json!({"type":"custom-title", "sessionId":sid,
        "customTitle":"unsent draft"});
    let mode = serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"code", "instructionEpoch":0});
    assert!(!super::session_state::is_mode_only_draft(
        &format!("{permission}\n"),
        &sid.to_string()
    ));
    for source in [
        format!("{mode}\n{permission}\n"),
        format!("{mode}\n{title}\n"),
        format!("{mode}\n{permission}\n{title}\n"),
    ] {
        assert!(super::session_state::is_mode_only_draft(
            &source,
            &sid.to_string()
        ));
        assert!(!super::session_state::is_mode_only_draft(
            source.trim_end_matches('\n'),
            &sid.to_string()
        ));
    }
    for bad in [
        serde_json::json!({"type":"permission-mode", "sessionId":sid}),
        serde_json::json!({"type":"custom-title", "sessionId":sid, "customTitle":{"role":"user"}}),
        serde_json::json!({"type":"unknown", "sessionId":sid}),
        serde_json::json!({"type":"user", "sessionId":sid, "message":{}}),
        serde_json::json!({"type":"custom-title", "sessionId":uuid::Uuid::new_v4(), "customTitle":"foreign"}),
    ] {
        assert!(!super::session_state::is_mode_only_draft(
            &format!("{mode}\n{permission}\n{bad}\n"),
            &sid.to_string()
        ));
    }
    assert!(!super::session_state::is_mode_only_draft(
        "",
        &sid.to_string()
    ));
    assert!(!super::session_state::is_mode_only_draft(
        "\n",
        &sid.to_string()
    ));
}

#[tokio::test]
async fn captured_startup_hook_draft_restores_fixed_code_before_hooks() {
    use super::{
        hydrate_captured_session_before_start, DesktopConfig, DesktopSessionResumeSnapshot,
    };
    use lingxi_core::host::OrchestratorHandle;
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    let root = tempfile::tempdir().unwrap();
    let id = lingxi_core::types::SessionId::new();
    let sid = id.as_uuid().to_string();
    let source = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"fixed-session-mode", "sessionId":sid, "sessionMode":"code", "instructionEpoch":0}),
        serde_json::json!({
            "type":"attachment", "sessionId":sid, "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
            "isSidechain":false, "cwd":root.path(), "version":"1", "timestamp":"2026-10-28T00:00:00Z",
            "attachment":{"type":"hook_additional_context", "hookName":"SessionStart",
                "hookEvent":"SessionStart", "toolUseID":"SessionStart", "content":["startup context"]}
        })
    );
    let cfg = DesktopConfig {
        cwd: root.path().into(),
        session_id_override: Some(sid.clone()),
        session_resume_snapshot: Some(DesktopSessionResumeSnapshot {
            source_session_id: sid,
            content: source.into(),
        }),
        ..Default::default()
    };
    let orch = orchestrator::ConversationOrchestrator::new(
        orchestrator::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().into(),
    )
    .with_session_id(id);
    assert_eq!(
        hydrate_captured_session_before_start(
            &orch,
            &cfg,
            lingxi_core::host::ModelProvenance::ProviderCatalogTier
        )
        .await
        .unwrap(),
        "resume"
    );
    let profile = orch.conversation_mode().await;
    assert_eq!(
        profile.mode,
        lingxi_core::host::session_context::ConversationMode::Code
    );
    assert!(orch.session().lock().await.history.is_empty());
}

#[async_trait::async_trait]
impl lingxi_core::host::PermissionGate for ColdResumePermissionGate {
    async fn check(
        &self,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::host::PermissionDecision {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        lingxi_core::host::PermissionDecision::Deny {
            reason: "permission transport fixture".into(),
        }
    }
}

#[cfg(unix)]
struct ResolvedModeGate(std::sync::RwLock<String>);

#[cfg(unix)]
#[async_trait::async_trait]
impl lingxi_core::host::PermissionGate for ResolvedModeGate {
    async fn check(
        &self,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::host::PermissionDecision {
        lingxi_core::host::PermissionDecision::Allow
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
        *self.0.write().unwrap() = mode.to_owned();
        Ok(())
    }

    fn permission_mode(&self) -> Option<String> {
        Some(self.0.read().unwrap().clone())
    }
}

#[tokio::test]
async fn captured_cold_history_waits_for_transport_before_dispatching_deferred_tools() {
    use super::{
        hydrate_captured_session_before_start, DesktopConfig, DesktopSessionResumeSnapshot,
    };
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = tempfile::tempdir().unwrap();
    let id = lingxi_core::types::SessionId::new();
    let user = uuid::Uuid::new_v4();
    let assistant = uuid::Uuid::new_v4();
    let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
    let rows = [
        serde_json::json!({"type":"fixed-session-mode","sessionId":id.as_uuid(),"sessionMode":"code","instructionEpoch":0}),
        serde_json::json!({"type":"user","uuid":user,"sessionId":id.as_uuid(),"parentUuid":null,"message":{"role":"user","content":"pending tool"}}),
        serde_json::json!({"type":"assistant","uuid":assistant,"sessionId":id.as_uuid(),"parentUuid":user,"message":{"role":"assistant","content":[{"type":"tool_use","id":"deferred_cold","name":"NeedsApproval","input":{}}]}}),
        serde_json::json!({"type":"attachment","uuid":uuid::Uuid::new_v4(),"sessionId":id.as_uuid(),"parentUuid":assistant,"attachment":{"type":"hook_deferred_tool","toolUseID":"deferred_cold","toolName":"NeedsApproval","toolInput":{}}}),
    ];
    let content = rows
        .iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
    std::fs::write(&path, &content).unwrap();
    let cfg = DesktopConfig {
        cwd: root.path().into(),
        session_id_override: Some(id.as_uuid().to_string()),
        session_transcript_path: Some(path.clone()),
        session_resume_snapshot: Some(DesktopSessionResumeSnapshot {
            source_session_id: id.as_uuid().to_string(),
            content: content.clone().into(),
        }),
        ..Default::default()
    };
    let permissions = Arc::new(AtomicUsize::new(0));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut tools = tool_api::ToolRegistry::new();
    tools.register_builtin(Arc::new(ColdResumeTool(dispatches.clone())));
    let fs: Arc<dyn FileSystem> =
        Arc::new(super::platform::DesktopFileSystem::new(root.path().into()));
    let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs));
    writer
        .bootstrap_session_message_identity_snapshot(&path)
        .await
        .unwrap();
    let orchestrator = Arc::new(
        orchestrator::ConversationOrchestrator::new(
            orchestrator::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(ColdResumePermissionGate(permissions.clone())),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().into(),
        )
        .with_session_id(id)
        .with_jsonl_writer(writer),
    );
    orchestrator.attach_owned_session_switches();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        hydrate_captured_session_before_start(
            &orchestrator,
            &cfg,
            lingxi_core::host::ModelProvenance::ProviderCatalogTier,
        ),
    )
    .await
    .expect("cold hydration must not wait for a permission client")
    .unwrap();
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    let loaded = session::jsonl::route_lines(&content);
    let pending = orchestrator::deferred_tool_replays_from_messages(&loaded.messages_in_order);
    assert_eq!(pending.len(), 1);
    assert!(orchestrator
        .complete_cold_resume_deferred_tools(id, root.path().join("wrong.jsonl"), pending.clone())
        .await
        .is_err());
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    orchestrator
        .complete_cold_resume_deferred_tools(id, path, pending)
        .await
        .unwrap();
    assert_eq!(permissions.load(Ordering::SeqCst), 1);
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        0,
        "the connected permission fixture denied execution"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn selected_cold_snapshot_is_hydrated_before_session_start_appends() {
    use super::{
        configure_session_hook_root, hydrate_captured_session_before_start, DesktopConfig,
        DesktopSessionResumeSnapshot,
    };
    use orchestrator::test_support::{MockApiClient, MockOutputStream, StaticMemoryProvider};

    let root = tempfile::tempdir().unwrap();
    let launch = root.path().join("launch");
    let target = root.path().join("target");
    let source_bucket = root.path().join("selected-copy");
    for directory in [&launch, &target, &source_bucket] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let launch = launch.canonicalize().unwrap();
    let target = target.canonicalize().unwrap();
    let source_bucket = source_bucket.canonicalize().unwrap();
    let home = root.path().canonicalize().unwrap().join("home");
    let session_id = lingxi_core::types::SessionId::new();
    let uuid = uuid::Uuid::new_v4();
    let selected = source_bucket.join(format!("{}.jsonl", session_id.as_uuid()));
    let unselected = session::jsonl::session_path(
        &home,
        &target.to_string_lossy(),
        &session_id.as_uuid().to_string(),
    );
    std::fs::create_dir_all(unselected.parent().unwrap()).unwrap();
    let content = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"fixed-session-mode", "sessionId":session_id.as_uuid(), "sessionMode":"chat", "instructionEpoch":1}),
        serde_json::json!({
            "type":"user", "uuid":uuid, "sessionId":session_id.as_uuid(),
            "parentUuid":null, "cwd":target,
            "message":{"role":"user","content":"selected-history"}
        }),
    );
    std::fs::write(&selected, &content).unwrap();
    std::fs::write(
        &unselected,
        content.replace("selected-history", "unselected-history"),
    )
    .unwrap();
    let unselected_bytes = std::fs::read(&unselected).unwrap();
    let unselected_mtime = std::fs::metadata(&unselected).unwrap().modified().unwrap();
    let cfg = DesktopConfig {
        cwd: target.clone(),
        initial_project_root: Some(launch.clone()),
        lingxi_home: home.clone(),
        session_id_override: Some(session_id.as_uuid().to_string()),
        session_transcript_path: Some(selected.clone()),
        session_resume_snapshot: Some(DesktopSessionResumeSnapshot {
            source_session_id: session_id.as_uuid().to_string(),
            content: content.clone().into(),
        }),
        permission_mode: permission::PermissionMode::Auto,
        permission_mode_cli_explicit: true,
        ..Default::default()
    };
    let input_path = root.path().join("hook-input.json");
    let environment_path = root.path().join("hook-environment.txt");
    let response = serde_json::json!({"hookSpecificOutput":{
        "hookEventName":"SessionStart", "additionalContext":"selected-hook-context",
        "initialUserMessage":"selected-hook-prompt"
    }});
    let command = format!(
        "cat > '{}'; printf '%s\\n%s\\n' \"${project_dir_env}\" \"$PWD\" > '{}'; printf '%s\\n' '{}'",
        input_path.display(), environment_path.display(), response,
        project_dir_env = branding::PROJECT_DIR_ENV,
    );
    let definitions = hooks::parse_hooks_from_settings_json(
        &serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{
            "type":"command", "command":command
        }]}]}})
        .to_string(),
        hooks::HookSource::Settings(lingxi_core::types::SettingsScope::Project),
    )
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    for hook in definitions {
        registry.register(hook);
    }
    let hooks = Arc::new(configure_session_hook_root(
        hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(platform_posix::PosixHttp::new()),
            Arc::new(platform_posix::PosixRuntime::new()),
        )
        .with_process_runner(
            Arc::new(super::platform::DesktopProcess::new()),
            Arc::new(platform_posix::PosixSandbox::new()),
        ),
        &cfg,
    ));
    let fs: Arc<dyn FileSystem> = Arc::new(super::platform::DesktopFileSystem::new(target.clone()));
    let writer = Arc::new(session::JsonlWriter::new(selected.clone(), fs));
    writer
        .bootstrap_session_message_identity_snapshot(&selected)
        .await
        .unwrap();
    let orchestrator = orchestrator::ConversationOrchestrator::new(
        orchestrator::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        hooks,
        Arc::new(ResolvedModeGate(std::sync::RwLock::new("plan".into()))),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        target.clone(),
    )
    .with_session_id(session_id)
    .with_config_home(home)
    .with_jsonl_writer(writer);
    let deferred = DesktopConfig {
        defer_session_start: true,
        ..cfg.clone()
    };
    hydrate_captured_session_before_start(
        &orchestrator,
        &deferred,
        lingxi_core::host::ModelProvenance::ProviderCatalogTier,
    )
    .await
    .unwrap();
    assert!(orchestrator.session().lock().await.history.is_empty());
    assert_eq!(std::fs::read_to_string(&selected).unwrap(), content);
    let source = hydrate_captured_session_before_start(
        &orchestrator,
        &cfg,
        lingxi_core::host::ModelProvenance::ProviderCatalogTier,
    )
    .await
    .unwrap();
    assert_eq!(source, "resume");
    assert_eq!(
        orchestrator.permission_mode().as_deref(),
        Some("plan"),
        "resume retains the resolved gate mode rather than raw configuration"
    );
    assert_eq!(
        orchestrator.session().lock().await.history[0].text_content(),
        "selected-history"
    );
    let mode = lingxi_core::host::OrchestratorHandle::conversation_mode(&orchestrator).await;
    assert_eq!(
        mode.mode,
        lingxi_core::host::session_context::ConversationMode::Chat,
        "the captured profile is active before SessionStart hooks"
    );
    let effects = orchestrator.fire_session_start(source).await;
    assert!(effects.initial_user_message.is_some());
    let input: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(input_path).unwrap()).unwrap();
    assert_eq!(input["source"], "resume");
    assert_eq!(input["cwd"], target.to_string_lossy().as_ref());
    assert_eq!(
        input["transcript_path"],
        selected.to_string_lossy().as_ref()
    );
    assert_eq!(
        std::fs::read_to_string(environment_path).unwrap(),
        format!("{}\n{}\n", launch.display(), target.display())
    );
    let persisted = std::fs::read_to_string(&selected).unwrap();
    assert!(persisted.contains("selected-hook-context"));
    assert!(persisted.contains("selected-hook-prompt"));
    let loaded = session::jsonl::route_lines(&persisted);
    let first_append = loaded
        .messages_in_order
        .iter()
        .find(|row| row.uuid != uuid.to_string())
        .unwrap();
    assert_eq!(
        first_append.parent_uuid.as_deref(),
        Some(uuid.to_string().as_str())
    );
    let history = orchestrator.session().lock().await.history.clone();
    assert!(history
        .iter()
        .any(|message| message.text_content() == "selected-hook-prompt"));
    assert_eq!(std::fs::read(&unselected).unwrap(), unselected_bytes);
    assert_eq!(
        std::fs::metadata(&unselected).unwrap().modified().unwrap(),
        unselected_mtime
    );
}

#[cfg(unix)]
#[tokio::test]
async fn remount_loads_target_project_settings_but_retains_original_hook_project_root() {
    use super::{configure_session_hook_root, load_effective_settings_for_config, DesktopConfig};

    let root = tempfile::tempdir().unwrap();
    let launch = root.path().join("launch");
    let target = root.path().join("target");
    for (path, label) in [(&launch, "A"), (&target, "B")] {
        std::fs::create_dir_all(path.join(branding::DOT_DIR)).unwrap();
        let command = format!(
            "printf '{label}-hook\\n%s\\n%s\\n' \"${project_dir_env}\" \"$PWD\"",
            project_dir_env = branding::PROJECT_DIR_ENV,
        );
        std::fs::write(
            path.join(branding::DOT_DIR).join("settings.json"),
            serde_json::to_string(&serde_json::json!({
                "model":format!("model-{label}"),
                "hooks":{"SessionStart":[{"hooks":[{"type":"command","command":command}]}]}
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let launch = launch.canonicalize().unwrap();
    let target = target.canonicalize().unwrap();
    let launch_cfg = DesktopConfig {
        cwd: launch.clone(),
        lingxi_home: root.path().join("isolated-home"),
        setting_source_scope: (false, true),
        ..Default::default()
    };
    assert_eq!(launch_cfg.initial_project_root, None);

    let compose_hooks = |cfg: &DesktopConfig| {
        let source =
            std::fs::read_to_string(cfg.cwd.join(branding::DOT_DIR).join("settings.json")).unwrap();
        let definitions = hooks::parse_hooks_from_settings_json(
            &source,
            hooks::HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        let mut registry = hooks::HookRegistry::new();
        for hook in definitions {
            registry.register(hook);
        }
        configure_session_hook_root(
            hooks::HookExecutorImpl::new(
                Arc::new(tokio::sync::RwLock::new(registry)),
                Arc::new(platform_posix::PosixHttp::new()),
                Arc::new(platform_posix::PosixRuntime::new()),
            )
            .with_process_runner(
                Arc::new(super::platform::DesktopProcess::new()),
                Arc::new(platform_posix::PosixSandbox::new()),
            ),
            cfg,
        )
    };
    let session_id = lingxi_core::types::SessionId::new();
    let startup = hooks::HookEvent::SessionStart {
        session_id,
        source: "resume".into(),
    };
    let hooks = compose_hooks(&launch_cfg);
    let launched = hooks
        .execute(
            startup.clone(),
            hooks::HookContext {
                session_id,
                cwd: launch.clone(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(launched.all_results.len(), 1);
    assert_eq!(
        launched.all_results[0].1.stdout.display.trim_end(),
        format!("A-hook\n{}\n{}", launch.display(), launch.display()),
    );
    drop(hooks);

    let remount_cfg = DesktopConfig {
        cwd: target.clone(),
        initial_project_root: Some(launch.clone()),
        ..launch_cfg
    };
    let settings = load_effective_settings_for_config(&remount_cfg, None).unwrap();
    assert_eq!(settings.settings.model.as_deref(), Some("model-B"));
    let cwd = tool_api::SessionCwd::new(target.clone(), vec![target.clone()]);
    assert_eq!(cwd.project_root(), target);
    let hooks = compose_hooks(&remount_cfg);
    let remounted = hooks
        .execute(
            startup,
            hooks::HookContext {
                session_id,
                cwd: cwd.cwd(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(remounted.all_results.len(), 1);
    assert_eq!(
        remounted.all_results[0].1.stdout.display.trim_end(),
        format!("B-hook\n{}\n{}", launch.display(), target.display()),
        "target hooks run in B with the original session hook project root A",
    );
}

#[tokio::test]
async fn missing_cold_worktree_is_cleared_in_the_selected_transcript() {
    let root = tempfile::tempdir().unwrap();
    let boot = root.path().join("boot");
    std::fs::create_dir(&boot).unwrap();
    let id = uuid::Uuid::new_v4();
    let path = root.path().join(format!("{id}.jsonl"));
    let state =
        serde_json::json!({"originalCwd":boot,"worktreePath":root.path().join("missing-worktree")});
    let content = format!(
        "{}\n",
        serde_json::json!({"type":"worktree-state","sessionId":id,"worktreeSession":state})
    );
    std::fs::write(&path, &content).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(super::platform::DesktopFileSystem::new(boot.clone()));
    let writer = session::JsonlWriter::new(path.clone(), fs);
    let cwd = tool_api::SessionCwd::new(boot.clone(), vec![boot.clone()]);
    let cell = tool_api::worktree_session::new_worktree_session_cell();
    restore_session_worktree_state(state, true, &writer, &cwd, &cell).await;
    assert!(cell.lock().unwrap().is_none());
    assert_eq!(cwd.cwd(), boot);
    let loaded = session::jsonl::route_lines(&std::fs::read_to_string(path).unwrap());
    assert_eq!(
        loaded.worktree_states[&id.to_string()],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn cold_worktree_restore_keeps_extra_grants_and_ephemeral_restore_never_writes() {
    let root = tempfile::tempdir().unwrap();
    let boot = root.path().join("boot");
    let worktree = root.path().join("worktree");
    let additional = root.path().join("explicit");
    std::fs::create_dir(&boot).unwrap();
    std::fs::create_dir(&worktree).unwrap();
    let path = root.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
    let content = "original-source\n";
    std::fs::write(&path, content).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(super::platform::DesktopFileSystem::new(boot.clone()));
    let writer = session::JsonlWriter::new(path.clone(), fs);
    let cwd = tool_api::SessionCwd::new(boot.clone(), vec![boot.clone(), additional.clone()]);
    let cell = tool_api::worktree_session::new_worktree_session_cell();
    restore_session_worktree_state(
        serde_json::json!({"originalCwd":boot,"worktreePath":worktree}),
        true,
        &writer,
        &cwd,
        &cell,
    )
    .await;
    assert_eq!(cwd.cwd(), worktree);
    assert!(cwd.trusted_dirs().contains(&additional));
    assert!(cell.lock().unwrap().is_some());
    let empty = tool_api::worktree_session::new_worktree_session_cell();
    restore_session_worktree_state(
        serde_json::json!({"originalCwd":boot,"worktreePath":root.path().join("missing")}),
        false,
        &writer,
        &cwd,
        &empty,
    )
    .await;
    assert_eq!(std::fs::read_to_string(path).unwrap(), content);
}
#[tokio::test]
async fn fresh_desktop_prompt_publishes_writer_authority_for_capture_and_fork() {
    use lingxi_core::host::session_context::ConversationMode;
    use lingxi_core::host::OrchestratorHandle;

    let (_root, mut cfg) = super::tests::test_config(true);
    let id = lingxi_core::types::SessionId::new();
    cfg.session_id_override = Some(id.as_uuid().to_string());
    cfg.initial_conversation_mode = Some(ConversationMode::Code);
    cfg.enable_automation_scheduler = false;
    cfg.isolated_credential_storage = true;
    let runtime = Box::pin(super::build(
        cfg,
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(ScheduledAnchorPermissionSink),
    ))
    .await
    .unwrap();
    runtime
        .orchestrator
        .initialize_conversation_mode()
        .await
        .unwrap();
    let writer = runtime.fork_writer.as_ref().unwrap();
    let row: session::JsonlMessage = serde_json::from_value(serde_json::json!({
        "type":"user", "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
        "sessionId":id.as_uuid(), "cwd":runtime.session_cwd.cwd(),
        "message":{"role":"user", "content":"fresh desktop prompt"}
    }))
    .unwrap();
    writer.append(&row).await.unwrap();
    let authority = writer
        .active_transcript_authority()
        .expect("fresh writer authority");
    writer.verify_active_transcript_source(&authority).unwrap();
    let lease = runtime.current_session_writer_lease(id).await.unwrap();
    assert_eq!(lease.canonical_session_id(), Some(id));
    let capture = runtime
        .capture_current_session_transcript(id)
        .await
        .unwrap();
    assert_eq!(capture.transcript_path, authority.path());
    assert_eq!(capture.source.as_bytes(), authority.read_checked().unwrap());
    assert!(capture.source.contains("fresh desktop prompt"));
    drop(lease);
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}

#[tokio::test]
async fn desktop_cd_keeps_exact_source_across_cold_resume() {
    use lingxi_core::host::session_context::ConversationMode;
    let (_root, mut cfg) = super::tests::test_config(true);
    let id = uuid::Uuid::new_v4();
    cfg.session_id_override = Some(id.to_string());
    cfg.initial_conversation_mode = Some(ConversationMode::Chat);
    cfg.restore_draft = true;
    cfg.durable_draft_title = Some("Scheduled task".into());
    cfg.enable_automation_scheduler = false;
    cfg.isolated_credential_storage = true;
    let original_cwd = cfg.cwd.clone();
    let target = original_cwd.join("new-cwd");
    std::fs::create_dir_all(&target).unwrap();
    let target = target.canonicalize().unwrap();
    let source = session::jsonl::session_path(
        &cfg.lingxi_home,
        &original_cwd.to_string_lossy(),
        &id.to_string(),
    );
    let runtime = Box::pin(super::build(
        cfg.clone(),
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(ScheduledAnchorPermissionSink),
    ))
    .await
    .unwrap();
    let before = std::fs::metadata(&source).unwrap();
    runtime
        .change_working_directory(target.clone())
        .await
        .unwrap();
    assert_eq!(runtime.session_cwd.cwd(), target);
    assert_eq!(runtime.fork_writer.as_ref().unwrap().active_path(), source);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let after = std::fs::metadata(&source).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    }
    #[cfg(not(unix))]
    let _ = before;
    let destination =
        session::jsonl::session_path(&cfg.lingxi_home, &target.to_string_lossy(), &id.to_string());
    assert!(!destination.exists());
    let source_text = std::fs::read_to_string(&source).unwrap();
    let marker = source_text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row["type"] == "relocated" && row["sessionId"] == id.to_string())
        .last()
        .unwrap();
    assert_eq!(marker["relocatedCwd"].as_str(), target.to_str());
    let fs: Arc<dyn FileSystem> = Arc::new(super::platform::DesktopFileSystem::new(target.clone()));
    let rows =
        session::jsonl::list_recent_sessions(&cfg.lingxi_home, &target.to_string_lossy(), 20, fs)
            .await
            .unwrap();
    assert!(rows.iter().any(|row| row.uuid == id
        && row.path == source
        && row.mode == ConversationMode::Chat
        && row.workspace_path.as_deref() == target.to_str()));
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
    drop(runtime);

    let captured = std::fs::read_to_string(&source).unwrap();
    let physical_source = source.canonicalize().unwrap();
    cfg.lingxi_home = cfg.lingxi_home.canonicalize().unwrap();
    let (authority, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
        cfg.lingxi_home.clone(),
        physical_source
            .strip_prefix(&cfg.lingxi_home)
            .unwrap()
            .to_path_buf(),
    )
    .unwrap();
    cfg.cwd = target.clone();
    cfg.initial_project_root = Some(original_cwd);
    cfg.durable_draft_title = None;
    cfg.session_transcript_path = Some(authority.path());
    cfg.session_transcript_authority = Some(authority);
    cfg.session_resume_snapshot = Some(super::DesktopSessionResumeSnapshot {
        source_session_id: id.to_string(),
        content: captured.into(),
    });
    let resumed = Box::pin(super::build(
        cfg,
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(ScheduledAnchorPermissionSink),
    ))
    .await
    .unwrap();
    assert_eq!(resumed.session_cwd.cwd(), target);
    assert_eq!(
        resumed.fork_writer.as_ref().unwrap().active_path(),
        physical_source
    );
    assert!(
        resumed
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}
