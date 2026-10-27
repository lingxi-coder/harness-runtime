use super::restore_session_worktree_state;
use lingxi_core::host::FileSystem;
use std::sync::Arc;

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
    use orchestrator::test_support::{
        MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
    };

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
        "{}\n",
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
        Arc::new(NoOpPermissionGate),
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
        orchestrator.session().lock().await.history[0].text_content(),
        "selected-history"
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
