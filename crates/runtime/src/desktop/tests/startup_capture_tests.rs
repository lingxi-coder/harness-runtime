use super::{DesktopConfig, DesktopSessionResumeLoaded, DesktopSessionResumeSnapshot};
use lingxi_core::host::live_sessions::{SessionWriterLease, SharedSessionWriterLease};
use lingxi_core::host::{session_context::ConversationMode, OrchestratorHandle};
use std::sync::Arc;

struct TestLease(String);

impl SessionWriterLease for TestLease {
    fn session_id(&self) -> &str {
        &self.0
    }
}

struct PermissionSink;

#[async_trait::async_trait]
impl client::adapter::PermissionRequestSink for PermissionSink {
    async fn emit_request(&self, _: client::protocol::permission::PermissionRequest) {}
}

#[test]
fn runtime_controls_release_captures_without_consuming_construction_authority() {
    let id = lingxi_core::types::SessionId::new();
    let source: Arc<str> = "source startup bytes".into();
    let target: Arc<str> = "target startup bytes".into();
    let parsed = Arc::new(session::jsonl::LoadedTranscript::default());
    let source_weak = Arc::downgrade(&source);
    let target_weak = Arc::downgrade(&target);
    let parsed_weak = Arc::downgrade(&parsed);
    let lease: SharedSessionWriterLease = Arc::new(TestLease(id.to_string()));
    let lease_weak = Arc::downgrade(&lease);
    let mut cfg = DesktopConfig {
        session_id_override: Some(id.as_uuid().to_string()),
        session_resume_snapshot: Some(DesktopSessionResumeSnapshot {
            source_session_id: id.as_uuid().to_string(),
            content: source,
        }),
        session_resume_target_snapshot: Some(target),
        session_resume_cost: Some(cost::CostState {
            session_id: id,
            total_nano_usd: 37,
            ..Default::default()
        }),
        session_writer_lease: Some(lease),
        permission_mode: permission::PermissionMode::Plan,
        ..Default::default()
    };
    cfg.session_resume_loaded = Some(DesktopSessionResumeLoaded::new(
        cfg.session_resume_snapshot.as_ref().unwrap(),
        parsed,
    ));

    let controls = cfg.clone_for_runtime_controls();
    assert!(controls.session_resume_snapshot.is_none());
    assert!(controls.session_resume_loaded.is_none());
    assert!(controls.session_resume_target_snapshot.is_none());
    assert!(controls.session_resume_cost.is_none());
    assert!(controls.session_writer_lease.is_none());
    assert_eq!(controls.session_id_override, cfg.session_id_override);
    assert_eq!(controls.permission_mode, permission::PermissionMode::Plan);
    assert_eq!(cfg.session_resume_cost.as_ref().unwrap().total_nano_usd, 37);
    assert_eq!(lease_weak.strong_count(), 1);
    assert!(source_weak.upgrade().is_some());
    assert!(target_weak.upgrade().is_some());
    assert!(parsed_weak.upgrade().is_some());
    drop(cfg);
    assert!(source_weak.upgrade().is_none());
    assert!(target_weak.upgrade().is_none());
    assert!(parsed_weak.upgrade().is_none());
    assert!(lease_weak.upgrade().is_none());
    assert_eq!(controls.permission_mode, permission::PermissionMode::Plan);
}

#[tokio::test]
async fn replaced_startup_capture_never_reuses_stale_prepared_history() {
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };

    let root = tempfile::tempdir().unwrap();
    let id = lingxi_core::types::SessionId::new();
    let source = |text| -> Arc<str> {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(),
                "sessionMode":"code", "instructionEpoch":0}),
            serde_json::json!({"type":"user", "sessionId":id.as_uuid(),
                "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
                "message":{"role":"user", "content":text}}),
        )
        .into()
    };
    let snapshot = DesktopSessionResumeSnapshot {
        source_session_id: id.as_uuid().to_string(),
        content: source("old history"),
    };
    let parsed = Arc::new(session::jsonl::route_lines(&snapshot.content));
    let prepared = DesktopSessionResumeLoaded::new(&snapshot, parsed.clone());
    let cfg = DesktopConfig {
        cwd: root.path().into(),
        session_id_override: Some(id.as_uuid().to_string()),
        session_resume_snapshot: Some(snapshot),
        session_resume_loaded: Some(prepared),
        ..Default::default()
    };
    assert!(std::ptr::eq(
        cfg.prepared_session_resume().unwrap(),
        parsed.as_ref()
    ));

    let mut replaced = cfg.clone();
    replaced
        .session_resume_snapshot
        .as_mut()
        .unwrap()
        .source_session_id = uuid::Uuid::new_v4().to_string();
    assert!(replaced.prepared_session_resume().is_none());
    let mut replaced = cfg.clone();
    let same_bytes = replaced
        .session_resume_snapshot
        .as_ref()
        .unwrap()
        .content
        .to_string();
    replaced.session_resume_snapshot.as_mut().unwrap().content = same_bytes.into();
    assert!(replaced.prepared_session_resume().is_none());
    replaced.session_resume_snapshot.as_mut().unwrap().content = source("new history");

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
        super::hydrate_captured_session_before_start(
            &orch,
            &replaced,
            lingxi_core::host::ModelProvenance::ProviderCatalogTier,
        )
        .await
        .unwrap(),
        "resume"
    );
    assert_eq!(
        orch.session().lock().await.history[0].text_content(),
        "new history"
    );
    assert!(cfg.prepared_session_resume().is_some());
}

#[tokio::test]
async fn resumed_desktop_releases_startup_captures_while_controls_and_writer_stay_live() {
    let (_root, mut cfg) = super::tests::test_config(true);
    cfg.cwd = cfg.cwd.canonicalize().unwrap();
    std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
    cfg.lingxi_home = cfg.lingxi_home.canonicalize().unwrap();
    cfg.isolated_credential_storage = true;
    let id = lingxi_core::types::SessionId::new();
    let sid = id.as_uuid().to_string();
    let path = session::jsonl::session_path(&cfg.lingxi_home, &cfg.cwd.to_string_lossy(), &sid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let source: Arc<str> = format!(
        "{}\n{}\n{}\n",
        serde_json::json!({"type":"fixed-session-mode", "sessionId":sid,
            "sessionMode":"chat", "instructionEpoch":1}),
        serde_json::json!({"type":"custom-title", "sessionId":sid,
            "customTitle":"Captured title", "cwd":cfg.cwd}),
        serde_json::json!({"type":"user", "sessionId":sid,
            "uuid":uuid::Uuid::new_v4(), "parentUuid":null, "cwd":cfg.cwd,
            "message":{"role":"user", "content":"captured conversation"}}),
    )
    .into();
    std::fs::write(&path, source.as_bytes()).unwrap();
    let (authority, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
        cfg.lingxi_home.clone(),
        path.strip_prefix(&cfg.lingxi_home).unwrap().to_path_buf(),
    )
    .unwrap();
    let parsed = Arc::new(session::jsonl::route_lines(&source));
    let parsed_weak = Arc::downgrade(&parsed);
    let source_weak = Arc::downgrade(&source);
    // An independent target allocation must also be released, even when this
    // ordinary resume ultimately selects the matching source as its target.
    let target: Arc<str> = source.to_string().into();
    let target_weak = Arc::downgrade(&target);
    let lease =
        lingxi_core::host::live_sessions::LiveSessionDir::at_live(cfg.lingxi_home.join("sessions"))
            .claim_session_id(&sid, std::process::id())
            .unwrap()
            .into_shared();
    let lease_weak = Arc::downgrade(&lease);
    cfg.session_id_override = Some(sid.clone());
    cfg.session_transcript_path = Some(path.clone());
    cfg.session_transcript_authority = Some(authority);
    cfg.session_writer_lease = Some(lease);
    cfg.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
        source_session_id: sid,
        content: source,
    });
    cfg.session_resume_loaded = Some(DesktopSessionResumeLoaded::new(
        cfg.session_resume_snapshot.as_ref().unwrap(),
        parsed,
    ));
    cfg.session_resume_target_snapshot = Some(target);
    cfg.session_resume_cost = Some(cost::CostState {
        session_id: id,
        total_nano_usd: 37,
        total_api_duration_ms: 19,
        ..Default::default()
    });
    let target_cwd = cfg.cwd.join("changed");
    std::fs::create_dir(&target_cwd).unwrap();
    let target_cwd = target_cwd.canonicalize().unwrap();

    let runtime = Box::pin(super::build(
        cfg,
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(PermissionSink),
    ))
    .await
    .unwrap();
    assert!(
        source_weak.upgrade().is_none(),
        "source bytes outlived startup"
    );
    assert!(
        target_weak.upgrade().is_none(),
        "target bytes outlived startup"
    );
    assert!(
        parsed_weak.upgrade().is_none(),
        "parsed source outlived startup"
    );
    assert!(
        lease_weak.upgrade().is_some(),
        "active writer lost its claim"
    );
    assert_eq!(runtime.session_state.hydration_replay_count(), 2);
    assert_eq!(
        runtime.orchestrator.conversation_mode().await.mode,
        ConversationMode::Chat
    );
    assert_eq!(
        runtime.orchestrator.session().lock().await.history[0].text_content(),
        "captured conversation"
    );
    let cost = runtime.session_lifecycle.cost_tracker.snapshot().await;
    assert_eq!(cost.session_id, id);
    assert_eq!(cost.total_nano_usd, 37);
    assert_eq!(cost.total_api_duration_ms, 19);
    assert_eq!(cost.cost_revision, 1);
    assert!(cost.legacy_import_evaluated);
    let settings = runtime.command_visibility_snapshot_loader();
    settings().unwrap();
    runtime
        .change_working_directory(target_cwd.clone())
        .await
        .unwrap();
    assert_eq!(runtime.session_cwd.cwd(), target_cwd);
    settings().unwrap();
    assert_eq!(runtime.fork_writer.as_ref().unwrap().active_path(), path);
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("Captured title"));
    let report = runtime.session_lifecycle.shutdown_and_drain().await;
    assert!(report.complete, "{:?}", report.errors);
    drop(settings);
    drop(runtime);
    assert!(lease_weak.upgrade().is_none());
}
