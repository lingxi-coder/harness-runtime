//! Cold session preparation shared by all headless wire formats.

use crate::desktop::{BuildError, DesktopConfig, DesktopRuntime, DesktopSessionResumeSnapshot};
use lingxi_core::host::live_sessions::{LiveSessionDir, SharedSessionWriterLease};
use lingxi_core::host::{FileSystem, OrchestratorHandle, OutputStream};
use session::jsonl::JsonlMessage;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

#[cfg(unix)]
use platform_posix::PosixFileSystem as HostFileSystem;
#[cfg(windows)]
use platform_windows::WindowsFileSystem as HostFileSystem;

/// Selects which persisted conversation, if any, a headless run starts from.
#[derive(Clone, Debug, Default)]
pub enum SessionStart {
    /// Start a new session without loading history.
    #[default]
    New,
    /// Resume the specified persisted session.
    Resume {
        /// UUID of the session to resume.
        session_id: Uuid,
        /// Optional message boundary for truncating the resumed transcript.
        resume_session_at: Option<String>,
        /// Optional turn boundary to discard while preparing the resume.
        resume_drops_turn: Option<String>,
        /// Create a forked session identity from the selected history.
        fork: bool,
    },
    /// Continue the most recent session for the configured working directory.
    Continue {
        /// Create a forked session identity instead of continuing in place.
        fork: bool,
    },
}

/// Loaded once before assembly; execution never re-reads a different history.
pub struct PreparedSession {
    source_id: Uuid,
    source_content: Arc<str>,
    mounted_content: Arc<str>,
    session_id: Uuid,
    fork: bool,
    messages: Vec<JsonlMessage>,
    entries: Vec<JsonlMessage>,
    preserve_boot_model: bool,
    explicit_permission_mode: bool,
}

/// Load and prepare the selected resume/continue session before runtime assembly.
///
/// New sessions return `Ok(None)`. Existing sessions load their transcript once,
/// configure the session identity and writer lease, and return the prepared history.
///
/// # Errors
///
/// Returns an error when the requested session or history cannot be found or read.
pub async fn prepare(
    config: &mut DesktopConfig,
    start: &SessionStart,
) -> Result<Option<PreparedSession>, String> {
    config.defer_session_start = true;
    let fs: Arc<dyn FileSystem> = Arc::new(HostFileSystem::new(config.cwd.clone()));
    let cwd = config.cwd.to_string_lossy().into_owned();
    let (source_id, at, drops, fork) = match start {
        SessionStart::New => return Ok(None),
        SessionStart::Resume {
            session_id,
            resume_session_at,
            resume_drops_turn,
            fork,
        } => (
            *session_id,
            resume_session_at.as_deref(),
            resume_drops_turn.as_deref(),
            *fork,
        ),
        SessionStart::Continue { fork } => {
            let rows = session::jsonl::loader::list_recent_sessions(
                &config.lingxi_home,
                &cwd,
                1,
                fs.clone(),
            )
            .await
            .map_err(|error| match error {
                session::jsonl::loader::LoaderError::EmptyDirectory => {
                    "No conversation found to continue".to_owned()
                }
                other => other.to_string(),
            })?;
            let id = rows
                .first()
                .ok_or("No conversation found to continue")?
                .uuid;
            (id, None, None, *fork)
        }
    };
    let session_id = if fork {
        match config.session_id_override.as_deref() {
            Some(raw) => lingxi_core::types::SessionId::parse_prefixed(raw)
                .ok_or("Invalid branch session ID. Must be a valid UUID.")?
                .as_uuid(),
            None => Uuid::new_v4(),
        }
    } else {
        source_id
    };
    if fork && session_id == source_id {
        return Err("A branch must use a different session ID from its source".into());
    }
    let missing = || format!("No conversation found with session ID: {source_id}");
    let source_path = session::jsonl::resolve_session_path(&config.lingxi_home, &cwd, source_id)
        .await
        .map_err(|error| match error {
            session::jsonl::loader::LoaderError::SessionNotFound { .. } => missing(),
            other => other.to_string(),
        })?;
    // Do not create a live-session directory for a nonexistent resume target.
    if !tokio::fs::try_exists(&source_path)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(missing());
    }
    let source_lease = claim_source(config, source_id)?;
    // Path discovery can race the old writer's final relocation; repeat it
    // under the claim, then retain this exact path throughout assembly.
    let source_path = session::jsonl::resolve_session_path(&config.lingxi_home, &cwd, source_id)
        .await
        .map_err(|error| match error {
            session::jsonl::loader::LoaderError::SessionNotFound { .. } => missing(),
            other => other.to_string(),
        })?;
    let content: Arc<str> = tokio::fs::read_to_string(&source_path)
        .await
        .map_err(|error| error.to_string())?
        .into();
    let loaded = session::jsonl::route_lines(&content);
    // Keep the selected file authoritative. Workspace restoration changes
    // runtime composition only; a missing workspace does not rehome JSONL.
    let resumed_cwd = if !fork {
        session::jsonl::loader::transcript_workspace_path(&source_path, &loaded)
            .map(std::path::PathBuf::from)
            .filter(|workspace| workspace.is_dir())
    } else {
        None
    };
    let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &source_id.to_string());
    if messages.is_empty() {
        return Err(missing());
    }
    let messages = super::resume_truncation::apply_truncating_resume(messages, at, drops)?;
    let resume_cost = crate::desktop::session_state::capture_resume_cost(
        &config.lingxi_home,
        source_id,
        source_lease.clone(),
        &content,
    )
    .await?;
    if fork {
        let recovery = crate::desktop::session_state::capture_branch_recovery(
            &config.lingxi_home,
            source_id,
            source_lease.clone(),
        )
        .await?;
        orchestrator::native_computer::validate_independent_branch(
            source_id,
            &content,
            &messages,
            recovery.as_ref(),
        )?;
    }
    let (target_path, target_lease) = if fork {
        let target_path =
            session::jsonl::session_path(&config.lingxi_home, &cwd, &session_id.to_string());
        match tokio::fs::symlink_metadata(&target_path).await {
            Ok(_) => return Err("The requested branch session ID already exists".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not inspect branch target: {error}")),
        }
        let target_lease = if config.session_persistence {
            Some(claim_session(&config.lingxi_home, session_id)?)
        } else {
            None
        };
        (target_path, target_lease)
    } else {
        (
            source_path.clone(),
            config.session_persistence.then(|| source_lease.clone()),
        )
    };
    let (mounted_content, messages, entries) = if fork {
        let source_cwd = session::jsonl::loader::transcript_workspace_path(&source_path, &loaded)
            .unwrap_or_else(|| config.cwd.to_string_lossy().into_owned());
        let options = session::branch::CapturedBranch {
            lingxi_home: &config.lingxi_home,
            source_cwd: &source_cwd,
            target_cwd: &cwd,
            source_session_id: source_id,
            target_session_id: session_id,
            source_content: &content,
            messages: &messages,
            custom_title: None,
            mode: config.initial_conversation_mode,
        };
        let mounted_content: Arc<str> = if config.session_persistence {
            session::branch::create_branch_from_captured(options, fs.clone())
                .await
                .map_err(|error| format!("could not create branch: {error}"))?;
            tokio::fs::read_to_string(&target_path)
                .await
                .map_err(|error| error.to_string())?
                .into()
        } else {
            session::branch::render_branch_from_captured(options, fs.clone())
                .await
                .map_err(|error| format!("could not create branch: {error}"))?
                .content
                .into()
        };
        let target = session::jsonl::route_lines(&mounted_content);
        let (target_messages, _) =
            session::jsonl::build_conversation_chain(&target, &session_id.to_string());
        (mounted_content, target_messages, target.messages_in_order)
    } else {
        (content.clone(), messages, loaded.messages_in_order)
    };
    config.session_id_override = Some(session_id.to_string());
    if let Some(workspace) = resumed_cwd {
        // Explicit relative --add-dir paths belong to the caller's launch
        // directory, whereas project settings below load from the restored cwd.
        config.add_dir = config
            .add_dir
            .iter()
            .map(|raw| {
                let raw = std::path::PathBuf::from(raw.to_string_lossy().trim());
                if let Ok(rest) = raw.strip_prefix("~") {
                    dirs::home_dir().map_or(raw.clone(), |home| home.join(rest))
                } else if raw.is_absolute() {
                    raw
                } else {
                    config.cwd.join(raw)
                }
            })
            .collect();
        config.cwd = workspace;
    }
    config.session_transcript_path = Some(target_path);
    config.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
        source_session_id: source_id.to_string(),
        content: content.clone(),
    });
    config.session_resume_target_snapshot = fork.then(|| mounted_content.clone());
    config.session_writer_lease = target_lease;
    config.session_resume_cost = resume_cost;
    if config.initial_effort.is_none() {
        config.initial_effort = orchestrator::runtime_metadata_from_messages(&messages).effort;
    }
    Ok(Some(PreparedSession {
        source_id,
        source_content: content,
        mounted_content,
        session_id,
        fork,
        messages,
        entries,
        preserve_boot_model: config.default_model_explicit
            || config.default_model_env_pinned
            || config.cli_agent.is_some(),
        explicit_permission_mode: config.permission_mode_cli_explicit
            || config.permission_mode_preference.is_some(),
    }))
}

fn claim_source(
    config: &DesktopConfig,
    source_id: Uuid,
) -> Result<SharedSessionWriterLease, String> {
    if let Some(lease) = config.session_writer_lease.as_ref() {
        if lease.canonical_session_id().map(|id| id.as_uuid()) != Some(source_id) {
            return Err("resume source does not match the supplied session writer claim".into());
        }
        return Ok(lease.clone());
    }
    claim_session(&config.lingxi_home, source_id)
}

fn claim_session(home: &Path, session_id: Uuid) -> Result<SharedSessionWriterLease, String> {
    LiveSessionDir::at_live(home.join("sessions"))
        .claim_session_id(&session_id.to_string(), std::process::id())
        .map(|claim| claim.into_shared())
        .map_err(|error| format!("session writer claim failed: {error}"))
}

/// Assembly failure together with the status of draining a partially built runtime.
#[derive(Debug)]
pub struct HeadlessBuildError {
    /// The original desktop runtime build failure.
    pub error: BuildError,
    /// Retained cleanup owner when shutdown did not complete.
    pub cleanup: Option<super::HeadlessCleanup>,
    /// Shutdown result for the partially assembled runtime.
    pub shutdown: super::HeadlessShutdownStatus,
}

impl std::fmt::Display for HeadlessBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.error)
    }
}
impl std::error::Error for HeadlessBuildError {}

/// Build once with the selected durable identity, restore, and only then run
/// startup hooks. Failed restores still drain the assembled resource owner.
pub async fn build_prepared(
    config: DesktopConfig,
    prepared: Option<PreparedSession>,
    output: Arc<dyn OutputStream>,
    permission_sink: Arc<dyn client::adapter::PermissionRequestSink>,
) -> Result<DesktopRuntime, HeadlessBuildError> {
    let runtime = crate::desktop::build(config.clone(), output, permission_sink)
        .await
        .map_err(|error| HeadlessBuildError {
            error,
            cleanup: None,
            shutdown: super::HeadlessShutdownStatus::NotStarted,
        })?;
    if let Err(error) = mount_prepared(&runtime, &config, prepared).await {
        let shutdown = runtime.session_lifecycle.shutdown_and_drain().await;
        let shutdown_status = if shutdown.complete {
            super::HeadlessShutdownStatus::Complete
        } else {
            super::HeadlessShutdownStatus::Incomplete
        };
        let cleanup = if shutdown.complete {
            None
        } else {
            Some(super::cleanup::retain_cleanup(Arc::new(runtime), shutdown))
        };
        return Err(HeadlessBuildError {
            error: BuildError::DurableSession(error),
            cleanup,
            shutdown: shutdown_status,
        });
    }
    Ok(runtime)
}

async fn mount_prepared(
    runtime: &DesktopRuntime,
    config: &DesktopConfig,
    prepared: Option<PreparedSession>,
) -> Result<(), String> {
    let source = if let Some(prepared) = prepared {
        let replayed = orchestrator::state_from_messages(prepared.session_id, &prepared.messages);
        let permission_mode = if !prepared.explicit_permission_mode && replayed.plan_mode {
            permission::PermissionMode::Plan
        } else {
            config.permission_mode
        };
        let profile = session::JsonlWriter::session_mode_transition_from_source(
            &prepared.mounted_content,
            &prepared.session_id.to_string(),
        );
        // Preserve the headless resume's established model selection rule even
        // when the transcript has no human-turn model setting. Install it
        // before restoration so dependent reasoning state uses the same model.
        if !prepared.preserve_boot_model
            && runtime.model_provenance == lingxi_core::host::ModelProvenance::ProviderCatalogTier
        {
            let session = runtime.orchestrator.session();
            let mut state = session.lock().await;
            state.model = replayed.model;
            state.model_profile = replayed.model_profile;
        }
        runtime
            .orchestrator
            .restore_captured_session_history(
                prepared.session_id,
                &prepared.messages,
                &prepared.entries,
                true,
                permission_mode,
                profile,
                &prepared.mounted_content,
            )
            .await?;
        if !prepared.fork {
            orchestrator::replay_deferred_tools_after_resume(
                &runtime.orchestrator,
                orchestrator::deferred_tool_replays_from_messages(&prepared.entries),
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        "resume"
    } else {
        if let Some(mode) = config.initial_conversation_mode {
            runtime
                .orchestrator
                .set_conversation_mode(mode)
                .await
                .map_err(|error| error.to_string())?;
        }
        "startup"
    };
    let effects = runtime.orchestrator.fire_session_start(source).await;
    let _ = runtime.apply_session_start_effects(&effects).await;
    runtime.orchestrator.fire_instructions_loaded().await;
    if source == "resume" {
        if let Some(mode) = config.initial_conversation_mode {
            runtime
                .orchestrator
                .set_conversation_mode(mode)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Restore file snapshots to the selected user message in a prepared resume.
///
/// This operation requires a non-new prepared session and a user-message UUID
/// present in that session's loaded history.
///
/// # Errors
///
/// Returns an error when no session was prepared, the message is not a user
/// message in the session, or the file-history restore fails.
pub async fn rewind_files(
    config: &DesktopConfig,
    prepared: Option<&PreparedSession>,
    message_id: &str,
) -> Result<session::file_history::RewindOutcome, String> {
    let prepared = prepared.ok_or("--rewind-files requires --resume")?;
    let invalid = || {
        format!(
            "--rewind-files requires a user message UUID, but {message_id} is not a user message in this session"
        )
    };
    if !prepared
        .messages
        .iter()
        .any(|row| row.uuid == message_id && row.message_type == "user")
    {
        return Err(invalid());
    }
    let message_id = Uuid::parse_str(message_id).map_err(|_| invalid())?;
    let history = session::FileHistory::new(
        config.lingxi_home.clone(),
        config.cwd.clone(),
        prepared.source_id.to_string(),
    );
    history.restore_from_records(session::file_history::parse_snapshot_records(
        &prepared.source_content,
    ));
    history.rewind_files(message_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::session_context::{ConversationMode, SessionModeSnapshot};
    use lingxi_core::types::{utf16_json::Utf16JsonProjection, ToolUseId};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum StartupEvent {
        Mode(ConversationMode),
        Tool(String),
        Hook(String),
    }

    #[derive(Default)]
    struct StartupTrace(std::sync::Mutex<Vec<StartupEvent>>);

    #[async_trait::async_trait]
    impl OutputStream for StartupTrace {
        async fn emit_conversation_mode(&self, state: &SessionModeSnapshot) {
            self.0
                .lock()
                .unwrap()
                .push(StartupEvent::Mode(state.active));
        }

        async fn emit_hook_started(&self, _: &str, _: &str, event: &str) {
            self.0
                .lock()
                .unwrap()
                .push(StartupEvent::Hook(event.into()));
        }

        async fn emit_text(&self, _: &Utf16JsonProjection) {}

        async fn emit_tool_call(&self, _: &ToolUseId, tool: &str, _: &Utf16JsonProjection) {
            self.0.lock().unwrap().push(StartupEvent::Tool(tool.into()));
        }

        async fn emit_tool_result(
            &self,
            _: &ToolUseId,
            _: &str,
            _: &Utf16JsonProjection,
            _: &Utf16JsonProjection,
            _: Option<&Utf16JsonProjection>,
        ) {
        }

        async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
    }

    fn startup_config(root: &Path, id: Uuid) -> DesktopConfig {
        let cwd = root.join("startup-project");
        std::fs::create_dir_all(cwd.join(".lingxi")).unwrap();
        std::fs::write(
            cwd.join(".lingxi/settings.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        )
        .unwrap();
        let mut config = DesktopConfig::default();
        config.cwd = cwd;
        config.lingxi_home = root.join("startup-home");
        config.session_id_override = Some(id.to_string());
        config.isolated_credential_storage = true;
        config.use_noop_permission_gate = true;
        config.enable_automation_scheduler = false;
        config
    }

    fn startup_sink() -> Arc<dyn client::adapter::PermissionRequestSink> {
        Arc::new(crate::headless::HeadlessPermissionRequestSink)
    }

    #[tokio::test]
    async fn fresh_explicit_chat_is_durable_before_desktop_and_headless_hooks() {
        for (headless, restore_draft) in [(false, false), (false, true), (true, false)] {
            let root = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let mut config = startup_config(root.path(), id);
            config.initial_conversation_mode = Some(ConversationMode::Chat);
            config.restore_draft = restore_draft;
            let trace = Arc::new(StartupTrace::default());
            let runtime = if headless {
                let prepared = prepare(&mut config, &SessionStart::New).await.unwrap();
                build_prepared(config.clone(), prepared, trace.clone(), startup_sink())
                    .await
                    .unwrap()
            } else {
                crate::desktop::build(config.clone(), trace.clone(), startup_sink())
                    .await
                    .unwrap()
            };
            let events = trace.0.lock().unwrap().clone();
            let mode = events
                .iter()
                .position(|event| *event == StartupEvent::Mode(ConversationMode::Chat))
                .unwrap();
            let hook = events
                .iter()
                .position(|event| *event == StartupEvent::Hook("SessionStart".into()))
                .unwrap();
            assert!(mode < hook, "{events:?}");
            let path = session::jsonl::session_path(
                &config.lingxi_home,
                &config.cwd.to_string_lossy(),
                &id.to_string(),
            );
            let source = std::fs::read_to_string(path).unwrap();
            let persisted =
                session::JsonlWriter::session_mode_transition_from_source(&source, &id.to_string())
                    .unwrap();
            assert_eq!(
                (persisted.active, persisted.selected, persisted.pending),
                (ConversationMode::Chat, ConversationMode::Chat, false)
            );
            assert!(
                runtime
                    .session_lifecycle
                    .shutdown_and_drain()
                    .await
                    .complete
            );
        }
    }

    #[tokio::test]
    async fn explicit_default_code_restarts_without_a_mode_record() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut config = startup_config(root.path(), id);
        config.initial_conversation_mode = Some(ConversationMode::Code);
        let hook = serde_json::json!({"hookSpecificOutput":{
            "hookEventName":"SessionStart", "additionalContext":"startup context"
        }});
        let command = format!("printf '%s\\n' '{hook}'");
        std::fs::write(
            config.cwd.join(".lingxi/settings.json"),
            serde_json::json!({
                "hooks":{"SessionStart":[{"hooks":[{"type":"command","command":command}]}]}
            })
            .to_string(),
        )
        .unwrap();
        let runtime = crate::desktop::build(
            config.clone(),
            Arc::new(StartupTrace::default()),
            startup_sink(),
        )
        .await
        .unwrap();
        assert!(
            runtime
                .session_lifecycle
                .shutdown_and_drain()
                .await
                .complete
        );
        drop(runtime);
        let path = session::jsonl::session_path(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            &id.to_string(),
        );
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(crate::desktop::session_state::is_mode_only_draft(
            &source,
            &id.to_string()
        ));
        assert!(session::JsonlWriter::session_mode_transition_from_source(
            &source,
            &id.to_string()
        )
        .is_none());
        config.initial_conversation_mode = Some(ConversationMode::Chat);
        config.restore_draft = true;
        config.session_transcript_path = Some(path);
        config.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
            source_session_id: id.to_string(),
            content: source.into(),
        });
        let trace = Arc::new(StartupTrace::default());
        let resumed = crate::desktop::build(config, trace.clone(), startup_sink())
            .await
            .unwrap();
        assert_eq!(
            resumed.orchestrator.conversation_mode().await.active,
            ConversationMode::Code
        );
        let events = trace.0.lock().unwrap().clone();
        let code = events
            .iter()
            .position(|event| *event == StartupEvent::Mode(ConversationMode::Code))
            .unwrap();
        let hook = events
            .iter()
            .position(|event| *event == StartupEvent::Hook("SessionStart".into()))
            .unwrap();
        assert!(code < hook, "{events:?}");
        assert!(
            resumed
                .session_lifecycle
                .shutdown_and_drain()
                .await
                .complete
        );
    }

    #[tokio::test]
    async fn resumed_headless_recovers_saved_profile_before_replay_and_override() {
        for explicit in [None, Some(ConversationMode::Code)] {
            let root = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let mut config = startup_config(root.path(), id);
            config.initial_conversation_mode = explicit;
            let user = Uuid::new_v4();
            let assistant = Uuid::new_v4();
            let rows = [
                serde_json::json!({"type":"user", "uuid":user, "sessionId":id, "parentUuid":null, "cwd":config.cwd, "message":{"role":"user","content":"continue"}}),
                serde_json::json!({"type":"assistant", "uuid":assistant, "sessionId":id, "parentUuid":user, "cwd":config.cwd, "message":{"role":"assistant","content":[{"type":"tool_use","id":"deferred", "name":"Bash", "input":{"command":"true"}}]}}),
                serde_json::json!({"type":"attachment", "uuid":Uuid::new_v4(), "sessionId":id, "parentUuid":assistant, "cwd":config.cwd, "attachment":{"type":"hook_deferred_tool","toolUseID":"deferred","toolName":"Bash","toolInput":{"command":"true"}}}),
                serde_json::json!({"type":"session-mode", "sessionId":id, "sessionMode":"chat", "contextRevision":3, "profileEpoch":3, "admittedMode":"chat", "admittedEpoch":3, "pending":false}),
            ];
            let path = session::jsonl::session_path(
                &config.lingxi_home,
                &config.cwd.to_string_lossy(),
                &id.to_string(),
            );
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                rows.iter()
                    .map(|row| format!("{row}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            let prepared = prepare(&mut config, &resume(id, false)).await.unwrap();
            let trace = Arc::new(StartupTrace::default());
            let runtime = build_prepared(config, prepared, trace.clone(), startup_sink())
                .await
                .unwrap();
            let events = trace.0.lock().unwrap().clone();
            let chat = events
                .iter()
                .position(|event| *event == StartupEvent::Mode(ConversationMode::Chat))
                .unwrap();
            let replay = events
                .iter()
                .position(|event| *event == StartupEvent::Tool("Bash".into()))
                .unwrap();
            let hook = events
                .iter()
                .position(|event| *event == StartupEvent::Hook("SessionStart".into()))
                .unwrap();
            assert!(chat < replay && replay < hook, "{events:?}");
            if explicit.is_some() {
                let code = events
                    .iter()
                    .position(|event| *event == StartupEvent::Mode(ConversationMode::Code))
                    .unwrap();
                assert!(hook < code, "{events:?}");
            } else {
                assert!(
                    !events.contains(&StartupEvent::Mode(ConversationMode::Code)),
                    "{events:?}"
                );
            }
            let source = std::fs::read_to_string(path).unwrap();
            let persisted =
                session::JsonlWriter::session_mode_transition_from_source(&source, &id.to_string())
                    .unwrap();
            assert_eq!(
                (persisted.active, persisted.revision, persisted.active_epoch),
                if explicit.is_some() {
                    (ConversationMode::Code, 4, 4)
                } else {
                    (ConversationMode::Chat, 3, 3)
                }
            );
            assert!(
                runtime
                    .session_lifecycle
                    .shutdown_and_drain()
                    .await
                    .complete
            );
        }
    }

    fn source_transcript(source_id: Uuid, cwd: &Path) -> String {
        let mut first = serde_json::json!({
            "parentUuid": null, "isSidechain": false, "userType": "external",
            "cwd": cwd, "sessionId": source_id.to_string(), "version": "test",
            "type": "user", "uuid": "first", "timestamp": "2026-10-07T12:00:00.000Z",
            "message": {"role": "user", "content": "inherited"},
        });
        let first_line = serde_json::to_string(&first).unwrap();
        first["parentUuid"] = serde_json::json!("first");
        first["uuid"] = serde_json::json!("second");
        first["message"]["content"] = serde_json::json!("tail");
        format!(
            "{first_line}\n{}\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::json!({"type": "agent-setting", "sessionId": source_id.to_string(), "agentSetting": "reviewer"})
        )
    }

    async fn fixture() -> (
        tempfile::TempDir,
        DesktopConfig,
        Uuid,
        std::path::PathBuf,
        String,
    ) {
        let root = tempfile::tempdir().unwrap();
        let mut config = DesktopConfig::default();
        config.cwd = root.path().join("project");
        config.lingxi_home = root.path().join("home");
        tokio::fs::create_dir_all(&config.cwd).await.unwrap();
        let source_id = Uuid::new_v4();
        let path = session::jsonl::session_path(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            &source_id.to_string(),
        );
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let content = source_transcript(source_id, &config.cwd);
        tokio::fs::write(&path, &content).await.unwrap();
        (root, config, source_id, path, content)
    }

    fn resume(source_id: Uuid, fork: bool) -> SessionStart {
        SessionStart::Resume {
            session_id: source_id,
            resume_session_at: None,
            resume_drops_turn: None,
            fork,
        }
    }

    #[tokio::test]
    async fn resume_keeps_one_claim_and_one_snapshot_for_history_and_metadata() {
        let (_root, mut config, source_id, path, content) = fixture().await;
        let prepared = prepare(&mut config, &resume(source_id, false))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config.session_transcript_path.as_ref(), Some(&path));
        assert!(claim_session(&config.lingxi_home, source_id).is_err());
        assert_eq!(prepared.messages.len(), 2);
        // An out-of-contract writer is used only to prove assembly never
        // re-reads mutable metadata after the captured resume was admitted.
        tokio::fs::write(&path, content.replace("reviewer", "different"))
            .await
            .unwrap();
        let snapshot = config.session_resume_snapshot.as_ref().unwrap();
        let loaded = session::jsonl::route_lines(&snapshot.content);
        assert_eq!(
            session::jsonl::loader::agent_resume_state_from_loaded(
                &loaded,
                &snapshot.source_session_id
            )
            .0
            .as_deref(),
            Some("reviewer")
        );
        drop(config);
        assert!(claim_session(&_root.path().join("home"), source_id).is_ok());
    }

    #[tokio::test]
    async fn cross_workspace_resume_restores_existing_cwd_without_rehoming_source() {
        let (_root, mut config, source_id, path, content) = fixture().await;
        let source_cwd = config.cwd.clone();
        let other = _root.path().join("other-workspace");
        std::fs::create_dir(&other).unwrap();
        config.cwd = other.clone();
        config.add_dir = vec![std::path::PathBuf::from("relative-explicit")];
        prepare(&mut config, &resume(source_id, false))
            .await
            .unwrap();
        assert_eq!(config.cwd, source_cwd);
        assert_eq!(config.add_dir, vec![other.join("relative-explicit")]);
        assert_eq!(config.session_transcript_path, Some(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert!(!session::jsonl::session_path(
            &config.lingxi_home,
            &other.to_string_lossy(),
            &source_id.to_string()
        )
        .exists());
    }

    #[tokio::test]
    async fn missing_resume_workspace_keeps_launch_cwd_and_authoritative_source_path() {
        let (_root, mut config, source_id, path, content) = fixture().await;
        std::fs::remove_dir(&config.cwd).unwrap();
        let other = _root.path().join("other-workspace");
        std::fs::create_dir(&other).unwrap();
        config.cwd = other.clone();
        prepare(&mut config, &resume(source_id, false))
            .await
            .unwrap();
        assert_eq!(config.cwd, other);
        assert_eq!(config.session_transcript_path, Some(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[tokio::test]
    async fn cold_resume_prefers_latest_relocated_cwd_in_the_captured_snapshot() {
        let (_root, mut config, source_id, path, mut content) = fixture().await;
        let target = _root.path().join("relocated");
        std::fs::create_dir(&target).unwrap();
        content.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type":"relocated", "sessionId":source_id, "relocatedCwd":target,
            })
        ));
        std::fs::write(&path, &content).unwrap();
        prepare(&mut config, &resume(source_id, false))
            .await
            .unwrap();
        assert_eq!(config.cwd, target);
        assert_eq!(config.session_transcript_path, Some(path));
        assert_eq!(
            config
                .session_resume_snapshot
                .as_ref()
                .unwrap()
                .content
                .as_ref(),
            content
        );
    }

    #[tokio::test]
    async fn fork_atomically_publishes_history_with_a_fresh_profile() {
        let (_root, mut config, source_id, source_path, source_content) = fixture().await;
        let prepared = prepare(&mut config, &resume(source_id, true))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(prepared.session_id, source_id);
        assert!(prepared.fork);
        assert!(claim_session(&config.lingxi_home, source_id).is_ok());
        assert!(claim_session(&config.lingxi_home, prepared.session_id).is_err());
        assert!(config.session_transcript_path.as_ref().unwrap().exists());
        assert!(config.parent_session_id.is_none());
        assert_eq!(prepared.messages.len(), 2);
        assert_eq!(prepared.messages[0].parent_uuid, None);
        assert_eq!(prepared.messages[1].parent_uuid.as_deref(), Some("first"));
        assert!(prepared
            .messages
            .iter()
            .all(|message| message.session_id == prepared.session_id.to_string()));
        let profile = session::JsonlWriter::session_mode_transition_from_source(
            &prepared.mounted_content,
            &prepared.session_id.to_string(),
        )
        .unwrap();
        assert_eq!(
            (profile.active, profile.active_epoch),
            (ConversationMode::Code, 1)
        );
        assert_eq!(
            config
                .session_resume_snapshot
                .as_ref()
                .unwrap()
                .source_session_id,
            source_id.to_string()
        );
        assert_eq!(
            tokio::fs::read_to_string(source_path).await.unwrap(),
            source_content
        );
    }

    #[tokio::test]
    async fn fork_uses_explicit_target_and_refuses_source_or_existing_target() {
        for persistence in [true, false] {
            let (_root, mut config, source_id, source_path, source) = fixture().await;
            config.session_persistence = persistence;
            let target_id = Uuid::new_v4();
            let target_id_text = target_id.to_string();
            config.session_id_override = Some(target_id_text.clone());
            let prepared = prepare(&mut config, &resume(source_id, true))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(prepared.session_id, target_id);
            assert_eq!(
                config.session_id_override.as_deref(),
                Some(target_id_text.as_str())
            );
            assert_eq!(
                config.session_transcript_path.as_ref().unwrap().exists(),
                persistence
            );
            assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);

            let (_root, mut collision, source_id, source_path, source) = fixture().await;
            collision.session_persistence = persistence;
            let target_id = Uuid::new_v4();
            collision.session_id_override = Some(target_id.to_string());
            let target = session::jsonl::session_path(
                &collision.lingxi_home,
                &collision.cwd.to_string_lossy(),
                &target_id.to_string(),
            );
            std::fs::write(&target, "existing target\n").unwrap();
            let error = prepare(&mut collision, &resume(source_id, true))
                .await
                .err()
                .unwrap();
            assert!(error.contains("already exists"), "{error}");
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "existing target\n"
            );
            assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);

            collision.session_id_override = Some(source_id.to_string());
            let error = prepare(&mut collision, &resume(source_id, true))
                .await
                .err()
                .unwrap();
            assert!(error.contains("different session ID"), "{error}");
            assert_eq!(std::fs::read_to_string(source_path).unwrap(), source);
        }
    }

    #[tokio::test]
    async fn fork_uses_fresh_code_epoch_and_never_reactivates_source_skill() {
        for persistence in [true, false] {
            let (_root, mut config, source_id, source_path, _) = fixture().await;
            config.session_persistence = persistence;
            config.initial_conversation_mode = Some(ConversationMode::Code);
            config.isolated_credential_storage = true;
            config.enable_automation_scheduler = false;
            let first = Uuid::new_v4();
            let skill = Uuid::new_v4();
            let tail = Uuid::new_v4();
            let rows = [
                serde_json::json!({"type":"user", "uuid":first, "sessionId":source_id, "parentUuid":null, "cwd":config.cwd, "message":{"role":"user","content":"question"}}),
                serde_json::json!({"type":"user", "uuid":skill, "sessionId":source_id, "parentUuid":first, "cwd":config.cwd, "isMeta":true, "origin":{"type":"skill_activation","context_epoch":7,"skill_name":"old","skill_path":"/skills/old"}, "message":{"role":"user","content":"old skill"}}),
                serde_json::json!({"type":"user", "uuid":tail, "sessionId":source_id, "parentUuid":skill, "cwd":config.cwd, "message":{"role":"user","content":"continue"}}),
                serde_json::json!({"type":"session-mode", "sessionId":source_id, "sessionMode":"chat", "contextRevision":3, "profileEpoch":3, "admittedMode":"chat", "admittedEpoch":3, "pending":false}),
            ];
            let source = rows
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>();
            std::fs::write(&source_path, &source).unwrap();
            let prepared = prepare(&mut config, &resume(source_id, true))
                .await
                .unwrap()
                .unwrap();
            let target_id = prepared.session_id;
            let profile = session::JsonlWriter::session_mode_transition_from_source(
                &prepared.mounted_content,
                &target_id.to_string(),
            )
            .unwrap();
            assert_eq!(
                (profile.active, profile.selected, profile.active_epoch),
                (ConversationMode::Code, ConversationMode::Code, 8)
            );
            let target_path = config.session_transcript_path.clone().unwrap();
            assert_eq!(target_path.exists(), persistence);
            assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);
            let runtime = build_prepared(
                config,
                Some(prepared),
                Arc::new(StartupTrace::default()),
                startup_sink(),
            )
            .await
            .unwrap();
            let history = runtime
                .orchestrator
                .session()
                .lock()
                .await
                .model_context_history();
            assert!(history
                .iter()
                .any(|message| message.text_content() == "question"));
            assert!(!history
                .iter()
                .any(|message| message.text_content() == "old skill"));
            let current = runtime.orchestrator.conversation_mode().await;
            assert_eq!(
                (current.active, current.selected, current.revision),
                (ConversationMode::Code, ConversationMode::Code, 8)
            );
            assert!(
                runtime
                    .session_lifecycle
                    .shutdown_and_drain()
                    .await
                    .complete
            );
            assert_eq!(std::fs::read_to_string(source_path).unwrap(), source);
            assert_eq!(target_path.exists(), persistence);
        }
    }

    #[tokio::test]
    async fn fork_rejects_deferred_source_work_without_writing_a_target() {
        let (_root, mut config, source_id, source_path, mut source) = fixture().await;
        let assistant = Uuid::new_v4();
        source.push_str(&format!("{}\n{}\n",
            serde_json::json!({"type":"assistant", "uuid":assistant, "sessionId":source_id, "parentUuid":"second", "message":{"role":"assistant","content":[{"type":"tool_use","id":"pending","name":"Bash","input":{"command":"true"}}]}}),
            serde_json::json!({"type":"attachment", "uuid":Uuid::new_v4(), "sessionId":source_id, "parentUuid":assistant, "attachment":{"type":"hook_deferred_tool","toolUseID":"pending","toolName":"Bash","toolInput":{"command":"true"}}})
        ));
        std::fs::write(&source_path, &source).unwrap();
        let files_before = std::fs::read_dir(source_path.parent().unwrap())
            .unwrap()
            .count();
        let error = prepare(&mut config, &resume(source_id, true))
            .await
            .err()
            .unwrap();
        assert!(
            error.contains("Resume the original session first"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);
        assert_eq!(
            std::fs::read_dir(source_path.parent().unwrap())
                .unwrap()
                .count(),
            files_before
        );
        assert!(config.session_id_override.is_none());
    }

    #[tokio::test]
    async fn missing_resume_does_not_create_a_session() {
        let root = tempfile::tempdir().unwrap();
        let mut config = DesktopConfig::default();
        config.cwd = root.path().join("project");
        config.lingxi_home = root.path().join("home");
        std::fs::create_dir_all(&config.cwd).unwrap();
        let id = Uuid::new_v4();
        let error = prepare(
            &mut config,
            &SessionStart::Resume {
                session_id: id,
                resume_session_at: None,
                resume_drops_turn: None,
                fork: false,
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            error,
            format!("No conversation found with session ID: {id}")
        );
        assert!(!config.lingxi_home.exists());
        assert!(config.session_id_override.is_none());
    }

    #[test]
    fn explicit_composition_does_not_follow_gate_shape() {
        let mut config = DesktopConfig::default();
        config.composition = Some(crate::desktop::DesktopSessionComposition::HeadlessCli);
        config.use_noop_permission_gate = false;
        assert_eq!(
            config.session_composition(),
            crate::desktop::DesktopSessionComposition::HeadlessCli
        );
    }
}
