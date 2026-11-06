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
    source_authority: session::jsonl::writer::TranscriptFileAuthority,
    source_lease: Option<SharedSessionWriterLease>,
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
    retain_source_for_rewind: bool,
) -> Result<Option<PreparedSession>, String> {
    config.defer_session_start = true;
    let fs: Arc<dyn FileSystem> = Arc::new(HostFileSystem::new(config.cwd.clone()));
    let cwd = config.cwd.to_string_lossy().into_owned();
    let (source_id, at, drops, fork, selected_path) = match start {
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
            config.session_transcript_path.clone(),
        ),
        SessionStart::Continue { fork } => {
            let catalog = session::jsonl::loader::list_recent_sessions_in_scope_with_mode(
                &config.lingxi_home,
                &cwd,
                session::jsonl::loader::SessionDiscoveryScope::Current,
                1,
                fs.clone(),
                config.initial_conversation_mode,
            )
            .await
            .map_err(|error| match error {
                session::jsonl::loader::LoaderError::EmptyDirectory => {
                    "No conversation found to continue".to_owned()
                }
                other => other.to_string(),
            })?;
            let selected = catalog
                .sessions
                .first()
                .ok_or("No conversation found to continue")?;
            (
                selected.uuid,
                None,
                None,
                *fork,
                Some(selected.path.clone()),
            )
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
    let (source_path, discovered_identity) = match selected_path {
        Some(path) => (path, None),
        None => {
            let (path, identity) =
                unique_resume_source(&config.lingxi_home, source_id, !fork).await?;
            (path, Some(identity))
        }
    };
    let (_, _, source_identity) = capture_resume_source(
        &config.lingxi_home,
        &source_path,
        source_id,
        discovered_identity,
        config.initial_conversation_mode,
        !fork,
    )
    .await?;
    if config
        .session_transcript_authority
        .as_ref()
        .is_some_and(|authority| authority != &source_identity)
    {
        return Err("captured resume authority changed before admission".into());
    }
    let source_path = source_identity.path();
    let source_lease = claim_source(config, source_id)?;
    // The lease never authorizes substituting another path or inode. Reopen
    // the exact source and compare both its directory and file identities.
    let (captured, loaded, _) = capture_resume_source(
        &config.lingxi_home,
        &source_path,
        source_id,
        Some(source_identity.clone()),
        config.initial_conversation_mode,
        !fork,
    )
    .await?;
    if let Some(snapshot) = config.session_resume_snapshot.as_ref() {
        if Uuid::parse_str(&snapshot.source_session_id).ok() != Some(source_id)
            || snapshot.content.as_ref() != captured.as_str()
        {
            return Err("captured resume source changed before admission".into());
        }
    }
    let content: Arc<str> = captured.into();
    let source_profile =
        session::JsonlWriter::fixed_session_mode_from_source(&content, &source_id.to_string())?;
    if config
        .initial_conversation_mode
        .is_some_and(|expected| expected != source_profile)
    {
        return Err(format!(
            "session mode mismatch: expected {}, found {}",
            config
                .initial_conversation_mode
                .expect("checked mode")
                .as_str(),
            source_profile.as_str()
        ));
    }
    config.initial_conversation_mode = Some(source_profile);
    let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &source_id.to_string());
    let empty_cwd = if messages.is_empty() {
        if !loaded
            .mobile_empty_sessions
            .contains(&source_id.to_string())
        {
            return Err(missing());
        }
        let workspace = session::jsonl::validated_empty_draft_cwd(&content, &source_id.to_string())
            .ok_or_else(missing)?;
        // A relocation explicitly changes the workspace; otherwise the
        // recorded cwd must own this transcript's project bucket.
        if session::jsonl::loader::transcript_workspace_path(&source_path, &loaded)
            .map(std::path::PathBuf::from)
            .as_deref()
            != Some(workspace.as_path())
        {
            return Err("empty session workspace does not match its transcript".into());
        }
        if !workspace.is_dir() {
            return Err("empty session workspace is not an existing directory".into());
        }
        if at.is_some() || drops.is_some() {
            return Err("cannot truncate an empty conversation".into());
        }
        Some(workspace)
    } else {
        None
    };
    // A draft anchor must identify its real workspace; never silently attach
    // an empty session to the launch cwd. Nonempty sessions retain their
    // existing missing-workspace behavior without rehoming the source file.
    let source_cwd = empty_cwd.or_else(|| {
        session::jsonl::loader::transcript_workspace_path(&source_path, &loaded)
            .map(std::path::PathBuf::from)
    });
    let resumed_cwd = (!fork)
        .then(|| source_cwd.clone())
        .flatten()
        .filter(|workspace| workspace.is_dir());
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
    let (mounted_content, messages, entries, branch_authority) = if fork {
        let source_cwd = source_cwd
            .map(|path| path.to_string_lossy().into_owned())
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
        let (mounted_content, branch_authority): (Arc<str>, _) = if config.session_persistence {
            let published = session::branch::create_branch_from_captured(options, fs.clone())
                .await
                .map_err(|error| format!("could not create branch: {error}"))?;
            (published.content.into(), Some(published.authority))
        } else {
            (
                session::branch::render_branch_from_captured(options, fs.clone())
                    .await
                    .map_err(|error| format!("could not create branch: {error}"))?
                    .content
                    .into(),
                None,
            )
        };
        let target = session::jsonl::route_lines(&mounted_content);
        let (target_messages, _) =
            session::jsonl::build_conversation_chain(&target, &session_id.to_string());
        (
            mounted_content,
            target_messages,
            target.messages_in_order,
            branch_authority,
        )
    } else {
        (content.clone(), messages, loaded.messages_in_order, None)
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
    let target_authority = if config.session_persistence && fork {
        let authority = branch_authority.ok_or("created branch has no publication authority")?;
        if authority
            .read_checked()
            .map_err(|error| error.to_string())?
            != mounted_content.as_bytes()
        {
            return Err("created branch target changed before admission".into());
        }
        Some(authority)
    } else if config.session_persistence {
        Some(source_identity.clone())
    } else {
        None
    };
    config.session_transcript_path = Some(
        target_authority
            .as_ref()
            .map_or(target_path, |authority| authority.path()),
    );
    config.session_transcript_authority = target_authority;
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
        source_authority: source_identity,
        source_lease: retain_source_for_rewind.then_some(source_lease),
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

async fn capture_resume_source(
    home: &Path,
    path: &Path,
    session_id: Uuid,
    expected: Option<session::jsonl::writer::TranscriptFileAuthority>,
    mode: Option<lingxi_core::host::session_context::ConversationMode>,
    _require_single_link: bool,
) -> Result<
    (
        String,
        session::jsonl::LoadedTranscript,
        session::jsonl::writer::TranscriptFileAuthority,
    ),
    String,
> {
    let path = path.to_path_buf();
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let logical_home = if home.is_absolute() {
            home
        } else {
            std::env::current_dir()
                .map_err(|error| error.to_string())?
                .join(home)
        };
        let root = std::fs::canonicalize(&logical_home).map_err(|error| error.to_string())?;
        let relative = path
            .strip_prefix(&logical_home)
            .or_else(|_| path.strip_prefix(&root))
            .map_err(|_| "resume source is outside the transcript home")?
            .to_path_buf();
        let components: Vec<_> = relative.components().collect();
        if components.len() != 3
            || components[0].as_os_str() != "projects"
            || components
                .iter()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            || path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| Uuid::parse_str(value).ok())
                != Some(session_id)
        {
            return Err("resume source does not match its catalog identity".into());
        }
        let (authority, bytes) = match expected {
            Some(authority) => {
                if authority.path() != root.join(&relative) {
                    return Err("resume source changed before writer admission".into());
                }
                let bytes = authority
                    .read_checked()
                    .map_err(|error| error.to_string())?;
                (authority, bytes)
            }
            None => session::jsonl::writer::TranscriptFileAuthority::capture(root, relative)
                .map_err(|error| error.to_string())?,
        };
        let content = String::from_utf8(bytes).map_err(|error| error.to_string())?;
        let loaded =
            session::jsonl::loader::route_captured_session_source(&path, session_id, &content)
                .map_err(|error| error.to_string())?;
        let actual = session::JsonlWriter::fixed_session_mode_from_source(
            &content,
            &session_id.to_string(),
        )?;
        if mode.is_some_and(|expected| expected != actual) {
            return Err(format!(
                "session mode mismatch: expected {}, found {}",
                mode.unwrap().as_str(),
                actual.as_str()
            ));
        }
        if authority
            .read_checked()
            .map_err(|error| error.to_string())?
            != content.as_bytes()
        {
            return Err("resume source changed during capture".into());
        }
        Ok((content, loaded, authority))
    })
    .await
    .map_err(|error| format!("resume source capture failed: {error}"))?
}

/// Resolve an unqualified UUID only when exactly one project file owns it.
async fn unique_resume_source(
    home: &Path,
    session_id: Uuid,
    require_single_link: bool,
) -> Result<
    (
        std::path::PathBuf,
        session::jsonl::writer::TranscriptFileAuthority,
    ),
    String,
> {
    let missing = || format!("No conversation found with session ID: {session_id}");
    let logical_projects = home.join("projects");
    let projects = tokio::fs::canonicalize(&logical_projects)
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                missing()
            } else {
                error.to_string()
            }
        })?;
    let mut entries = tokio::fs::read_dir(&projects)
        .await
        .map_err(|error| error.to_string())?;
    let mut selected = None;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| error.to_string())?
    {
        if !entry
            .file_type()
            .await
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let logical_bucket = logical_projects.join(entry.file_name());
        let mut files = tokio::fs::read_dir(entry.path())
            .await
            .map_err(|error| error.to_string())?;
        while let Some(file) = files
            .next_entry()
            .await
            .map_err(|error| error.to_string())?
        {
            let candidate = logical_bucket.join(file.file_name());
            if candidate.extension().and_then(|value| value.to_str()) != Some("jsonl")
                || candidate
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .filter(|value| session::jsonl::validate_uuid(value))
                    .and_then(|value| Uuid::parse_str(value).ok())
                    != Some(session_id)
            {
                continue;
            }
            match tokio::fs::symlink_metadata(&candidate).await {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(_) => return Err("resume source is not a regular file".into()),
                Err(error) => return Err(error.to_string()),
            }
            let (_, _, identity) = capture_resume_source(
                home,
                &candidate,
                session_id,
                None,
                None,
                require_single_link,
            )
            .await?;
            if selected.replace((candidate, identity)).is_some() {
                return Err(format!("ambiguous conversation session ID: {session_id}"));
            }
        }
    }
    selected.ok_or_else(missing)
}

/// Resolve a bare UUID only when exactly one physical project source exists.
/// The returned authority is the first opened, no-follow source identity.
pub async fn resolve_unique_resume_source(
    home: &Path,
    session_id: Uuid,
) -> Result<
    (
        std::path::PathBuf,
        session::jsonl::writer::TranscriptFileAuthority,
    ),
    String,
> {
    let (_, authority) = unique_resume_source(home, session_id, true).await?;
    Ok((authority.path(), authority))
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

pub(crate) async fn mount_prepared(
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
        let profile = session::JsonlWriter::fixed_session_mode_from_source(
            &prepared.mounted_content,
            &prepared.session_id.to_string(),
        )?;
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
        "startup"
    };
    runtime
        .orchestrator
        .initialize_conversation_mode()
        .await
        .map_err(|error| error.to_string())?;
    let effects = runtime.orchestrator.fire_session_start(source).await;
    let _ = runtime.apply_session_start_effects(&effects).await;
    runtime.orchestrator.fire_instructions_loaded().await;
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
    let loaded = session::jsonl::route_lines(&prepared.source_content);
    let workspace = session::jsonl::loader::transcript_workspace_path(
        &prepared.source_authority.path(),
        &loaded,
    )
    .ok_or("rewind source has no valid workspace")?;
    let mode = session::JsonlWriter::fixed_session_mode_from_source(
        &prepared.source_content,
        &prepared.source_id.to_string(),
    )?;
    session::file_history::rewind_from_captured(
        &config.lingxi_home,
        &workspace,
        prepared.source_id,
        mode,
        &prepared.source_authority,
        &prepared.source_content,
        message_id,
        prepared
            .source_lease
            .clone()
            .ok_or("session was not prepared for file rewind")?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::session_context::ConversationMode;
    use lingxi_core::types::{utf16_json::Utf16JsonProjection, ToolUseId};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum StartupEvent {
        Tool(String),
        Hook(String),
    }

    #[derive(Default)]
    struct StartupTrace(std::sync::Mutex<Vec<StartupEvent>>);

    #[async_trait::async_trait]
    impl OutputStream for StartupTrace {
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
                let prepared = prepare(&mut config, &SessionStart::New, false)
                    .await
                    .unwrap();
                build_prepared(config.clone(), prepared, trace.clone(), startup_sink())
                    .await
                    .unwrap()
            } else {
                crate::desktop::build(config.clone(), trace.clone(), startup_sink())
                    .await
                    .unwrap()
            };
            let events = trace.0.lock().unwrap().clone();
            assert!(events.contains(&StartupEvent::Hook("SessionStart".into())));
            let path = session::jsonl::session_path(
                &config.lingxi_home,
                &config.cwd.to_string_lossy(),
                &id.to_string(),
            );
            let source = std::fs::read_to_string(path).unwrap();
            assert_eq!(
                session::JsonlWriter::fixed_session_mode_from_source(&source, &id.to_string())
                    .unwrap(),
                ConversationMode::Chat
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
    async fn explicit_code_draft_has_fixed_header_and_rejects_chat_resume() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut config = startup_config(root.path(), id);
        config.initial_conversation_mode = Some(ConversationMode::Code);
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
        assert_eq!(
            session::JsonlWriter::fixed_session_mode_from_source(&source, &id.to_string()).unwrap(),
            ConversationMode::Code
        );
        config.initial_conversation_mode = Some(ConversationMode::Chat);
        config.restore_draft = true;
        config.session_transcript_path = Some(path);
        config.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
            source_session_id: id.to_string(),
            content: source.into(),
        });
        assert!(
            crate::desktop::build(config, Arc::new(StartupTrace::default()), startup_sink())
                .await
                .is_err()
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
                serde_json::json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"chat", "instructionEpoch":3}),
                serde_json::json!({"type":"user", "uuid":user, "sessionId":id, "parentUuid":null, "cwd":config.cwd, "message":{"role":"user","content":"continue"}}),
                serde_json::json!({"type":"assistant", "uuid":assistant, "sessionId":id, "parentUuid":user, "cwd":config.cwd, "message":{"role":"assistant","content":[{"type":"tool_use","id":"deferred", "name":"Bash", "input":{"command":"true"}}]}}),
                serde_json::json!({"type":"attachment", "uuid":Uuid::new_v4(), "sessionId":id, "parentUuid":assistant, "cwd":config.cwd, "attachment":{"type":"hook_deferred_tool","toolUseID":"deferred","toolName":"Bash","toolInput":{"command":"true"}}}),
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
            let prepared = prepare(&mut config, &resume(id, false), false).await;
            if explicit.is_some() {
                assert!(prepared.is_err());
                continue;
            }
            let prepared = prepared.unwrap();
            let trace = Arc::new(StartupTrace::default());
            let runtime = build_prepared(config, prepared, trace.clone(), startup_sink())
                .await
                .unwrap();
            let events = trace.0.lock().unwrap().clone();
            let replay = events
                .iter()
                .position(|event| *event == StartupEvent::Tool("Bash".into()))
                .unwrap();
            let hook = events
                .iter()
                .position(|event| *event == StartupEvent::Hook("SessionStart".into()))
                .unwrap();
            assert!(replay < hook, "{events:?}");
            let source = std::fs::read_to_string(path).unwrap();
            let persisted =
                session::JsonlWriter::fixed_session_profile_from_source(&source, &id.to_string())
                    .unwrap();
            assert_eq!(
                (persisted.mode, persisted.instruction_epoch),
                (ConversationMode::Chat, 3)
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
            "{}\n{first_line}\n{}\n{}\n",
            serde_json::json!({"type":"fixed-session-mode", "sessionId":source_id, "sessionMode":"code", "instructionEpoch":0}),
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
        let canonical_root = root.path().canonicalize().unwrap();
        let mut config = DesktopConfig::default();
        config.cwd = canonical_root.join("project");
        config.lingxi_home = canonical_root.join("home");
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
    async fn headless_rewind_keeps_captured_source_and_original_file_workspace() {
        for replace_source in [false, true] {
            let (root, mut config, id, path, content) = fixture().await;
            let first = Uuid::new_v4();
            let second = Uuid::new_v4();
            let content = content
                .replace("\"first\"", &format!("\"{first}\""))
                .replace("\"second\"", &format!("\"{second}\""));
            let a_file = config.cwd.join("foo");
            let b = root.path().canonicalize().unwrap().join("other");
            std::fs::create_dir(&b).unwrap();
            let b_file = b.join("foo");
            std::fs::write(&a_file, b"A before").unwrap();
            std::fs::write(&b_file, b"B business data").unwrap();
            let history = session::FileHistory::new(config.lingxi_home.clone(), id.to_string());
            history.make_snapshot(first).await;
            history.track_edit(a_file.to_str().unwrap()).await;
            std::fs::write(&a_file, b"A edited").unwrap();
            let snapshot = history.snapshot_record(first).unwrap();
            let content = format!(
                "{content}{}\n{}\n",
                session::file_history::snapshot_line_json(&id.to_string(), &snapshot),
                serde_json::json!({"type":"relocated", "sessionId":id, "relocatedCwd":b})
            );
            std::fs::write(&path, &content).unwrap();
            config.session_transcript_path = Some(path.clone());
            let prepared = prepare(&mut config, &resume(id, false), true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(config.cwd, b);
            if replace_source {
                std::fs::rename(&path, path.with_extension("retired")).unwrap();
                std::fs::write(&path, &content).unwrap();
            }
            let result = rewind_files(&config, Some(&prepared), &first.to_string()).await;
            if replace_source {
                assert!(result.is_err());
                assert_eq!(std::fs::read(&a_file).unwrap(), b"A edited");
            } else {
                assert!(result.is_ok(), "{result:?}");
                assert_eq!(std::fs::read(&a_file).unwrap(), b"A before");
            }
            assert_eq!(std::fs::read(&b_file).unwrap(), b"B business data");
        }
    }

    #[tokio::test]
    async fn actual_write_tool_binds_new_parent_for_safe_file_rewind() {
        use tool_api::tool_trait::Tool;
        for replace_parent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let root_path = root.path().canonicalize().unwrap();
            let a = root_path.join("a");
            let b = root_path.join("b");
            let home = root_path.join("home");
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            let path = a.join("new/deep/foo");
            std::fs::create_dir_all(b.join("new/deep")).unwrap();
            let other = b.join("new/deep/foo");
            std::fs::write(&other, b"B business data").unwrap();
            let history = Arc::new(session::FileHistory::new(
                home.clone(),
                "actual-write".into(),
            ));
            let first = Uuid::new_v4();
            history.make_snapshot(first).await;
            let builtin = tool_api::test_support::ctx_for_file_tools(
                tool_api::test_support::make_dummy_fs(),
                Arc::new(telemetry::AnalyticsBus::new()),
                vec![a],
            );
            let mut invocation = tool_api::test_support::fresh_ctx();
            invocation.file_history = Some(history.clone());
            tool_file::FileWriteTool::new(builtin)
                .call(
                    tool_api::tool_trait::generated_builtin_result(&serde_json::json!({
                        "file_path":path, "content":"new A file"
                    })),
                    invocation,
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            let record = history.snapshot_record(first).unwrap();
            let json = format!(
                "{}\n",
                session::file_history::snapshot_line_json("actual-write", &record)
            );
            let resumed = session::FileHistory::new(home, "actual-write".into());
            resumed
                .restore_from_records(session::file_history::parse_snapshot_records(&json).unwrap())
                .unwrap();
            if replace_parent {
                let parent = path.parent().unwrap();
                std::fs::rename(parent, root_path.join("retired")).unwrap();
                std::fs::create_dir(parent).unwrap();
                std::fs::write(&path, b"replacement business data").unwrap();
            }
            let outcome = resumed.rewind_files(first).await.unwrap();
            if replace_parent {
                assert_eq!(outcome.skipped_links, 1);
                assert_eq!(std::fs::read(&path).unwrap(), b"replacement business data");
                assert_eq!(
                    std::fs::read(root_path.join("retired/foo")).unwrap(),
                    b"new A file"
                );
            } else {
                assert_eq!(outcome.changed.len(), 1);
                assert!(!path.exists());
            }
            assert_eq!(std::fs::read(other).unwrap(), b"B business data");
        }
    }

    #[tokio::test]
    async fn headless_rewind_reports_partial_failure_instead_of_success() {
        let (_root, mut config, id, path, content) = fixture().await;
        let first = Uuid::new_v4();
        let content = content.replace("\"first\"", &format!("\"{first}\""));
        let good = config.cwd.join("good");
        let bad = config.cwd.join("bad");
        std::fs::write(&good, b"good before").unwrap();
        std::fs::write(&bad, b"bad before").unwrap();
        let history = session::FileHistory::new(config.lingxi_home.clone(), id.to_string());
        history.make_snapshot(first).await;
        history.track_edit(good.to_str().unwrap()).await;
        history.track_edit(bad.to_str().unwrap()).await;
        let record = history.snapshot_record(first).unwrap();
        let missing = record.tracked_file_backups[bad.to_str().unwrap()]
            .backup_file_name
            .as_ref()
            .unwrap();
        std::fs::remove_file(
            config
                .lingxi_home
                .join("file-history")
                .join(id.to_string())
                .join(missing),
        )
        .unwrap();
        std::fs::write(&good, b"good edited").unwrap();
        std::fs::write(&bad, b"bad edited").unwrap();
        let content = format!(
            "{content}{}\n",
            session::file_history::snapshot_line_json(&id.to_string(), &record)
        );
        std::fs::write(&path, &content).unwrap();
        config.session_transcript_path = Some(path.clone());
        let prepared = prepare(&mut config, &resume(id, false), true)
            .await
            .unwrap()
            .unwrap();
        let error = rewind_files(&config, Some(&prepared), &first.to_string())
            .await
            .unwrap_err();
        assert!(error.contains("partially restored"), "{error}");
        assert_eq!(std::fs::read(good).unwrap(), b"good before");
        assert_eq!(std::fs::read(bad).unwrap(), b"bad edited");
        assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    }

    #[tokio::test]
    async fn selected_source_path_wins_and_bare_duplicate_uuid_is_rejected() {
        let (_root, config, source_id, path, source) = fixture().await;
        let duplicate_dir = config.lingxi_home.join("projects").join("duplicate");
        std::fs::create_dir_all(&duplicate_dir).unwrap();
        let duplicate = duplicate_dir.join(format!("{source_id}.jsonl"));
        std::fs::write(&duplicate, source.replace("inherited", "other source")).unwrap();

        let mut ambiguous = config.clone();
        let error = prepare(&mut ambiguous, &resume(source_id, false), false)
            .await
            .err()
            .unwrap();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(ambiguous.session_writer_lease.is_none());

        let mut exact = config;
        exact.session_transcript_path = Some(path.clone());
        let prepared = prepare(&mut exact, &resume(source_id, false), false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            exact.session_transcript_path.as_deref(),
            Some(path.as_path())
        );
        assert_eq!(prepared.source_content.as_ref(), source);
        assert!(!prepared.source_content.contains("other source"));
        assert_eq!(
            std::fs::read_to_string(duplicate).unwrap(),
            source.replace("inherited", "other source")
        );
    }

    #[tokio::test]
    async fn selected_source_inode_is_rechecked_after_writer_claim() {
        let (_root, config, source_id, path, source) = fixture().await;
        let (_, _, pinned) = capture_resume_source(
            &config.lingxi_home,
            &path,
            source_id,
            None,
            Some(ConversationMode::Code),
            true,
        )
        .await
        .unwrap();
        let _lease = claim_source(&config, source_id).unwrap();
        let original = path.with_extension("original");
        std::fs::rename(&path, &original).unwrap();
        std::fs::write(&path, source.replace("inherited", "replacement")).unwrap();
        assert!(capture_resume_source(
            &config.lingxi_home,
            &path,
            source_id,
            Some(pinned),
            Some(ConversationMode::Code),
            true
        )
        .await
        .is_err());
        assert_eq!(std::fs::read_to_string(original).unwrap(), source);
    }

    #[tokio::test]
    async fn bare_uuid_preserves_uppercase_path_and_rejects_case_variant_duplicates() {
        let (_root, config, _, path, _) = fixture().await;
        let id = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let upper = path
            .parent()
            .unwrap()
            .join(format!("{}.jsonl", id.to_string().to_uppercase()));
        let source = source_transcript(id, &config.cwd);
        std::fs::write(&upper, &source).unwrap();
        let (selected, _) = unique_resume_source(&config.lingxi_home, id, true)
            .await
            .unwrap();
        assert_eq!(selected, upper);
        let other = config.lingxi_home.join("projects").join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(format!("{id}.jsonl")), source).unwrap();
        assert!(unique_resume_source(&config.lingxi_home, id, true)
            .await
            .err()
            .unwrap()
            .contains("ambiguous"));
    }

    #[tokio::test]
    async fn uppercase_filename_recovers_relocation_and_empty_anchor_workspace() {
        let (root, config, _, path, _) = fixture().await;
        std::fs::remove_file(path).unwrap();
        let id = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let upper = session::jsonl::session_path(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            &id.to_string().to_uppercase(),
        );
        let relocated = root.path().join("relocated-uppercase");
        std::fs::create_dir(&relocated).unwrap();
        let relocation =
            serde_json::json!({"type":"relocated","sessionId":id,"relocatedCwd":relocated});
        let source = format!("{}{relocation}\n", source_transcript(id, &config.cwd));
        std::fs::write(&upper, source).unwrap();
        let mut run = config.clone();
        prepare(&mut run, &resume(id, false), false).await.unwrap();
        assert_eq!(run.cwd, relocated);
        assert_eq!(
            run.session_transcript_path.as_deref(),
            Some(upper.as_path())
        );
        drop(run);

        let draft = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"fixed-session-mode","sessionId":id,"sessionMode":"code","instructionEpoch":0}),
            serde_json::json!({"type":"custom-title","sessionId":id,"customTitle":"Scheduled","mobileEmptySession":1,"cwd":config.cwd}),
        );
        std::fs::write(&upper, draft).unwrap();
        let catalog = session::jsonl::loader::list_recent_sessions_in_scope_with_mode(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            session::jsonl::loader::SessionDiscoveryScope::Current,
            1,
            Arc::new(HostFileSystem::new(config.cwd.clone())),
            Some(ConversationMode::Code),
        )
        .await
        .unwrap();
        assert_eq!(
            catalog.sessions[0].custom_or_ai_title.as_deref(),
            Some("Scheduled")
        );
        let mut run = config.clone();
        let prepared = prepare(&mut run, &SessionStart::Continue { fork: false }, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.source_id, id);
        assert_eq!(run.cwd, config.cwd);
        assert_eq!(
            run.session_transcript_path.as_deref(),
            Some(upper.as_path())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_hardlinked_resume_source_is_rejected_before_writing() {
        let (root, mut config, source_id, path, source) = fixture().await;
        let linked = root.path().join("other-transcript.jsonl");
        std::fs::hard_link(&path, &linked).unwrap();
        config.session_transcript_path = Some(path.clone());
        assert!(prepare(&mut config, &resume(source_id, false), false)
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
        assert_eq!(std::fs::read_to_string(linked).unwrap(), source);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_source_symlink_and_changed_captured_snapshot_are_rejected() {
        let (_root, config, source_id, path, source) = fixture().await;
        let link_dir = config.lingxi_home.join("projects").join("link");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join(format!("{source_id}.jsonl"));
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let mut linked = config.clone();
        linked.session_transcript_path = Some(link);
        assert!(prepare(&mut linked, &resume(source_id, false), false)
            .await
            .is_err());

        let mut captured = config;
        captured.session_transcript_path = Some(path.clone());
        captured.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
            source_session_id: source_id.to_string(),
            content: source.clone().into(),
        });
        std::fs::write(&path, source.replace("inherited", "changed")).unwrap();
        assert!(prepare(&mut captured, &resume(source_id, false), false)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn continue_selects_code_before_limiting_and_retains_its_path() {
        let (_root, mut config, source_id, path, _) = fixture().await;
        let chat_id = Uuid::new_v4();
        let chat_path = path.parent().unwrap().join(format!("{chat_id}.jsonl"));
        let chat = source_transcript(chat_id, &config.cwd)
            .replace("\"sessionMode\":\"code\"", "\"sessionMode\":\"chat\"")
            .replace("2026-10-07", "2026-10-09");
        std::fs::write(chat_path, chat).unwrap();
        config.initial_conversation_mode = Some(ConversationMode::Code);
        let prepared = prepare(&mut config, &SessionStart::Continue { fork: false }, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.source_id, source_id);
        assert_eq!(
            config.session_transcript_path.as_deref(),
            Some(path.as_path())
        );
        assert_eq!(
            config.initial_conversation_mode,
            Some(ConversationMode::Code)
        );
    }

    #[tokio::test]
    async fn continue_without_explicit_mode_restores_the_newest_session_mode() {
        let (_root, mut config, _, path, _) = fixture().await;
        let chat_id = Uuid::new_v4();
        let chat_path = path.parent().unwrap().join(format!("{chat_id}.jsonl"));
        let chat = source_transcript(chat_id, &config.cwd)
            .replace("\"sessionMode\":\"code\"", "\"sessionMode\":\"chat\"")
            .replace("2026-10-07", "2026-10-09");
        std::fs::write(&chat_path, chat).unwrap();
        let prepared = prepare(&mut config, &SessionStart::Continue { fork: false }, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.source_id, chat_id);
        assert_eq!(
            config.session_transcript_path.as_deref(),
            Some(chat_path.as_path())
        );
        assert_eq!(
            config.initial_conversation_mode,
            Some(ConversationMode::Chat)
        );
    }

    #[tokio::test]
    async fn resume_and_continue_accept_an_anchored_empty_code_draft() {
        let (_root, config, source_id, path, _) = fixture().await;
        let source = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"fixed-session-mode","sessionId":source_id,"sessionMode":"code","instructionEpoch":0}),
            serde_json::json!({"type":"custom-title","sessionId":source_id,"customTitle":"Scheduled","mobileEmptySession":1,"cwd":config.cwd}),
        );
        std::fs::write(&path, source).unwrap();
        for start in [
            resume(source_id, false),
            SessionStart::Continue { fork: false },
        ] {
            let mut run = config.clone();
            let prepared = prepare(&mut run, &start, false).await.unwrap().unwrap();
            assert!(prepared.messages.is_empty());
            assert_eq!(run.session_transcript_path.as_deref(), Some(path.as_path()));
        }
        let mut truncated = config;
        let error = prepare(
            &mut truncated,
            &SessionStart::Resume {
                session_id: source_id,
                resume_session_at: Some("missing".into()),
                resume_drops_turn: None,
                fork: false,
            },
            false,
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("truncate"));
    }

    #[tokio::test]
    async fn empty_draft_rejects_invalid_anchor_cwd_and_accepts_relocation() {
        let (root, config, source_id, path, _) = fixture().await;
        let header = serde_json::json!({"type":"fixed-session-mode","sessionId":source_id,"sessionMode":"code","instructionEpoch":0});
        let marker = serde_json::json!({"type":"custom-title","sessionId":source_id,"customTitle":"Scheduled","mobileEmptySession":1});
        for invalid in [None, Some(""), Some("relative")] {
            let mut row = marker.clone();
            if let Some(cwd) = invalid {
                row["cwd"] = serde_json::json!(cwd);
            }
            let source = format!("{header}\n{row}\n");
            std::fs::write(&path, &source).unwrap();
            let mut run = config.clone();
            run.session_transcript_path = Some(path.clone());
            assert!(prepare(&mut run, &resume(source_id, false), false)
                .await
                .is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }

        let old = root.path().join("old-workspace");
        std::fs::create_dir(&old).unwrap();
        let mut row = marker;
        row["cwd"] = serde_json::json!(old);
        let wrong_bucket = format!("{header}\n{row}\n");
        std::fs::write(&path, &wrong_bucket).unwrap();
        let mut run = config.clone();
        run.session_transcript_path = Some(path.clone());
        assert!(prepare(&mut run, &resume(source_id, false), false)
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), wrong_bucket);

        let relocated =
            serde_json::json!({"type":"relocated","sessionId":source_id,"relocatedCwd":config.cwd});
        let source = format!("{header}\n{row}\n{relocated}\n");
        std::fs::write(&path, &source).unwrap();
        let mut run = config.clone();
        run.session_transcript_path = Some(path.clone());
        let prepared = prepare(&mut run, &resume(source_id, false), false)
            .await
            .unwrap()
            .unwrap();
        assert!(prepared.messages.is_empty());
        assert_eq!(run.cwd, config.cwd);
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }

    #[tokio::test]
    async fn resume_keeps_one_claim_and_one_snapshot_for_history_and_metadata() {
        let (_root, mut config, source_id, path, content) = fixture().await;
        let prepared = prepare(&mut config, &resume(source_id, false), false)
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
        prepare(&mut config, &resume(source_id, false), false)
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
        prepare(&mut config, &resume(source_id, false), false)
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
        prepare(&mut config, &resume(source_id, false), false)
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
        let prepared = prepare(&mut config, &resume(source_id, true), false)
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
        let profile = session::JsonlWriter::fixed_session_profile_from_source(
            &prepared.mounted_content,
            &prepared.session_id.to_string(),
        )
        .unwrap();
        assert_eq!(
            (profile.mode, profile.instruction_epoch),
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
        drop(prepared);
        assert!(claim_session(&config.lingxi_home, source_id).is_ok());
    }

    #[tokio::test]
    async fn fork_uses_explicit_target_and_refuses_source_or_existing_target() {
        for persistence in [true, false] {
            let (_root, mut config, source_id, source_path, source) = fixture().await;
            config.session_persistence = persistence;
            let target_id = Uuid::new_v4();
            let target_id_text = target_id.to_string();
            config.session_id_override = Some(target_id_text.clone());
            let prepared = prepare(&mut config, &resume(source_id, true), false)
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
            let error = prepare(&mut collision, &resume(source_id, true), false)
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
            let error = prepare(&mut collision, &resume(source_id, true), false)
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
                serde_json::json!({"type":"fixed-session-mode", "sessionId":source_id, "sessionMode":"code", "instructionEpoch":3}),
                serde_json::json!({"type":"user", "uuid":first, "sessionId":source_id, "parentUuid":null, "cwd":config.cwd, "message":{"role":"user","content":"question"}}),
                serde_json::json!({"type":"user", "uuid":skill, "sessionId":source_id, "parentUuid":first, "cwd":config.cwd, "isMeta":true, "origin":{"type":"skill_activation","context_epoch":7,"skill_name":"old","skill_path":"/skills/old"}, "message":{"role":"user","content":"old skill"}}),
                serde_json::json!({"type":"user", "uuid":tail, "sessionId":source_id, "parentUuid":skill, "cwd":config.cwd, "message":{"role":"user","content":"continue"}}),
            ];
            let source = rows
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>();
            std::fs::write(&source_path, &source).unwrap();
            let prepared = prepare(&mut config, &resume(source_id, true), false)
                .await
                .unwrap()
                .unwrap();
            let target_id = prepared.session_id;
            let profile = session::JsonlWriter::fixed_session_profile_from_source(
                &prepared.mounted_content,
                &target_id.to_string(),
            )
            .unwrap();
            assert_eq!(
                (profile.mode, profile.instruction_epoch),
                (ConversationMode::Code, 8)
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
            assert_eq!(current.mode, ConversationMode::Code);
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
        let error = prepare(&mut config, &resume(source_id, true), false)
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
            false,
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
