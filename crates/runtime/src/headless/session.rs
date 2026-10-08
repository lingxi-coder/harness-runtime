//! Cold session preparation shared by all headless wire formats.

use crate::desktop::{BuildError, DesktopConfig, DesktopRuntime, DesktopSessionResumeSnapshot};
use lingxi_core::host::live_sessions::{LiveSessionDir, SharedSessionWriterLease};
use lingxi_core::host::{FileSystem, OutputStream};
use session::jsonl::JsonlMessage;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

#[cfg(unix)]
use platform_posix::PosixFileSystem as HostFileSystem;
#[cfg(windows)]
use platform_windows::WindowsFileSystem as HostFileSystem;

#[derive(Clone, Debug, Default)]
pub enum SessionStart {
    #[default]
    New,
    Resume {
        session_id: Uuid,
        resume_session_at: Option<String>,
        resume_drops_turn: Option<String>,
        fork: bool,
    },
    Continue {
        fork: bool,
    },
}

/// Loaded once before assembly; execution never re-reads a different history.
pub struct PreparedSession {
    source_id: Uuid,
    source_content: Arc<str>,
    session_id: Uuid,
    fork: bool,
    messages: Vec<JsonlMessage>,
    entries: Vec<JsonlMessage>,
    preserve_boot_model: bool,
    explicit_permission_mode: bool,
}

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
    let missing = || format!("No conversation found with session ID: {source_id}");
    let source_path =
        session::jsonl::resolve_session_path_across_worktrees(&config.lingxi_home, &cwd, source_id)
            .await
            .map_err(|error| error.to_string())?;
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
    let source_path =
        session::jsonl::resolve_session_path_across_worktrees(&config.lingxi_home, &cwd, source_id)
            .await
            .map_err(|error| error.to_string())?;
    let content: Arc<str> = tokio::fs::read_to_string(&source_path)
        .await
        .map_err(|error| error.to_string())?
        .into();
    let loaded = session::jsonl::route_lines(&content);
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
    let session_id = if fork { Uuid::new_v4() } else { source_id };
    let (target_path, target_lease) = if fork {
        let target_path =
            session::jsonl::session_path(&config.lingxi_home, &cwd, &session_id.to_string());
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
    config.session_id_override = Some(session_id.to_string());
    config.session_transcript_path = Some(target_path);
    config.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
        source_session_id: source_id.to_string(),
        content: content.clone(),
    });
    config.session_writer_lease = target_lease;
    config.session_resume_cost = resume_cost;
    if config.initial_effort.is_none() {
        config.initial_effort = orchestrator::runtime_metadata_from_messages(&messages).effort;
    }
    Ok(Some(PreparedSession {
        source_id,
        source_content: content,
        session_id,
        fork,
        messages,
        entries: loaded.messages_in_order,
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

#[derive(Debug)]
pub struct HeadlessBuildError {
    pub error: BuildError,
    pub cleanup: Option<super::HeadlessCleanup>,
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
        if prepared.fork {
            runtime.stage_fork_history(prepared.messages.clone()).await;
        }
        let replayed = orchestrator::state_from_messages(prepared.session_id, &prepared.messages);
        let permission_mode = if !prepared.explicit_permission_mode && replayed.plan_mode {
            permission::PermissionMode::Plan
        } else {
            config.permission_mode
        };
        runtime
            .orchestrator
            .set_permission_mode(permission_mode.wire_str())
            .await?;
        let last_uuid = prepared
            .messages
            .iter()
            .rev()
            .map(|row| row.uuid.clone())
            .find(|id| !id.is_empty());
        runtime.orchestrator.seed_last_jsonl_uuid(last_uuid).await;
        {
            let session = runtime.orchestrator.session();
            let mut state = session.lock().await;
            state.session_id = replayed.session_id;
            state.history = replayed.history;
            state.compacted_user_turns = replayed.compacted_user_turns;
            state.virtual_user_messages = replayed.virtual_user_messages;
            state.transcript_only_messages = replayed.transcript_only_messages;
            state.compact_summary_messages = replayed.compact_summary_messages;
            state.model_context_excluded_messages = replayed.model_context_excluded_messages;
            state.active_goal = replayed.active_goal;
            if !prepared.preserve_boot_model
                && runtime.model_provenance
                    == lingxi_core::host::ModelProvenance::ProviderCatalogTier
            {
                state.model = replayed.model;
                state.model_profile = replayed.model_profile;
            }
            state.plan_mode = permission_mode == permission::PermissionMode::Plan;
            if state.plan_mode {
                state.plan_reminder_shown = false;
            }
        }
        runtime
            .orchestrator
            .sync_active_goal_stop_hook_for_current_state()
            .await;
        runtime
            .orchestrator
            .restore_resume_runtime_metadata(&prepared.messages)
            .await;
        runtime
            .orchestrator
            .restore_resume_prompt_metadata(&prepared.entries)
            .await;
        orchestrator::replay_deferred_tools_after_resume(
            &runtime.orchestrator,
            orchestrator::deferred_tool_replays_from_messages(&prepared.entries),
        )
        .await
        .map_err(|error| error.to_string())?;
        "resume"
    } else {
        "startup"
    };
    let effects = runtime.orchestrator.fire_session_start(source).await;
    if effects.reload_skills {
        let handler = command_api::builtins::reload_skills::ReloadSkillsHandler::with_all_roots(
            runtime.shared_command_registry.clone(),
            config.cwd.clone(),
            config.lingxi_home.clone(),
            Some(crate::desktop::settings_watch::managed_settings_dir()),
            config
                .lingxi_home
                .parent()
                .unwrap_or(&config.lingxi_home)
                .to_path_buf(),
            Vec::new(),
            config.customization_gates.safe_mode,
        );
        if let Some(command) = command_api::parse_slash_command("/reload-skills") {
            use command_api::BuiltinCommandHandler;
            let _ = handler.handle(&command).await;
        }
    }
    runtime.orchestrator.fire_instructions_loaded().await;
    Ok(())
}

pub async fn rewind_files(
    config: &DesktopConfig,
    prepared: Option<&PreparedSession>,
    message_id: &str,
) -> Result<session::file_history::RewindOutcome, String> {
    let prepared = prepared.ok_or("--rewind-files requires --resume")?;
    let invalid = || {
        format!("--rewind-files requires a user message UUID, but {message_id} is not a user message in this session")
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
        let config = DesktopConfig {
            cwd: root.path().join("project"),
            lingxi_home: root.path().join("home"),
            ..DesktopConfig::default()
        };
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
    async fn fork_stages_history_without_writing_before_first_prompt_admission() {
        let (_root, mut config, source_id, source_path, source_content) = fixture().await;
        let prepared = prepare(&mut config, &resume(source_id, true))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(prepared.session_id, source_id);
        assert!(prepared.fork);
        assert!(claim_session(&config.lingxi_home, source_id).is_ok());
        assert!(claim_session(&config.lingxi_home, prepared.session_id).is_err());
        assert!(!config.session_transcript_path.as_ref().unwrap().exists());
        assert!(config.parent_session_id.is_none());
        assert_eq!(prepared.messages.len(), 2);
        assert_eq!(prepared.messages[0].parent_uuid, None);
        assert_eq!(prepared.messages[1].parent_uuid.as_deref(), Some("first"));
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
    async fn missing_resume_does_not_create_a_session() {
        let root = tempfile::tempdir().unwrap();
        let mut config = DesktopConfig {
            cwd: root.path().join("project"),
            lingxi_home: root.path().join("home"),
            ..DesktopConfig::default()
        };
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
        assert!(!error.is_empty());
        assert!(!config.lingxi_home.exists());
        assert!(config.session_id_override.is_none());
    }

    #[test]
    fn explicit_composition_does_not_follow_gate_shape() {
        let config = DesktopConfig {
            composition: Some(crate::desktop::DesktopSessionComposition::HeadlessCli),
            use_noop_permission_gate: false,
            ..DesktopConfig::default()
        };
        assert_eq!(
            config.session_composition(),
            crate::desktop::DesktopSessionComposition::HeadlessCli
        );
    }
}
