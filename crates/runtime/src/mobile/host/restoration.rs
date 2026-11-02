use client::protocol::listings::SessionModeDto;

/// A single validated capture used throughout a cold mobile mount. Neither a
/// client-supplied transcript path nor a sanitized directory name identifies a
/// workspace: both the transcript and cwd must come from the catalog record.
pub(super) struct InitialMobileSession {
    pub id: uuid::Uuid,
    pub source: String,
    pub raw_source: Vec<u8>,
    pub transcript_identity: lingxi_core::host::rooted_fs::RootIdentity,
    pub mode: lingxi_core::host::session_context::ConversationMode,
    pub messages: Vec<session::jsonl::JsonlMessage>,
    pub replayed: orchestrator::ReplayedSession,
    pub permission_mode: Option<String>,
    pub project_root: std::path::PathBuf,
    pub host_cwd: std::path::PathBuf,
    pub logical_cwd: std::path::PathBuf,
}

pub(super) async fn capture_initial_mobile_session(
    cfg: &super::MobileConfig,
    platform: &std::sync::Arc<dyn lingxi_core::host::Platform>,
) -> Result<Option<InitialMobileSession>, super::MobileBuildError> {
    use std::path::{Path, PathBuf};

    let Some(requested) = cfg.initial_session_id.as_deref() else {
        return Ok(None);
    };
    let fail = |reason: String| {
        super::MobileBuildError::Orchestrator(format!("initial session resume: {reason}"))
    };
    if cfg.app_agent_host {
        return Err(fail("Local App Agent cannot mount a conversation".into()));
    }
    let id = uuid::Uuid::parse_str(requested.strip_prefix("sess:").unwrap_or(requested))
        .map_err(|error| fail(format!("invalid session UUID: {error}")))?;
    let id_text = id.to_string();
    let catalog = cfg.lingxi_home.join("projects");
    let entries = std::fs::read_dir(&catalog)
        .map_err(|error| fail(format!("session catalog unavailable: {error}")))?;
    let canonical_catalog = catalog
        .canonicalize()
        .map_err(|error| fail(format!("invalid session catalog: {error}")))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| fail(format!("session catalog: {error}")))?;
        if !entry
            .file_type()
            .map_err(|error| fail(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        for source in std::fs::read_dir(entry.path())
            .map_err(|error| fail(format!("session catalog: {error}")))?
        {
            let source = source.map_err(|error| fail(format!("session catalog: {error}")))?;
            let candidate = source.path();
            if candidate.extension().and_then(|name| name.to_str()) != Some("jsonl")
                || candidate
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .and_then(|name| uuid::Uuid::parse_str(name).ok())
                    != Some(id)
            {
                continue;
            }
            if !source
                .file_type()
                .map_err(|error| fail(error.to_string()))?
                .is_file()
            {
                return Err(fail("session source is not a regular file".into()));
            }
            let path = candidate
                .canonicalize()
                .map_err(|error| fail(format!("session source unavailable: {error}")))?;
            if !path
                .strip_prefix(&canonical_catalog)
                .ok()
                .is_some_and(|relative| relative.components().count() == 2)
            {
                return Err(fail("session source is outside catalog".into()));
            }
            candidates.push(path);
        }
    }
    let path = match candidates.as_slice() {
        [path] => path.clone(),
        [] => return Err(fail(format!("session {id_text} was not found"))),
        _ => {
            return Err(fail(format!(
                "session {id_text} has ambiguous catalog sources"
            )))
        }
    };
    let model_fs = platform.filesystem();
    let capture_path = path.clone();
    let (raw_source, transcript_identity) = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let root = capture_path
            .parent()
            .ok_or_else(|| "transcript has no parent".to_string())?;
        let relative = Path::new(
            capture_path
                .file_name()
                .ok_or_else(|| "transcript has no filename".to_string())?,
        );
        let identity =
            lingxi_core::host::rooted_fs::root_identity(root).map_err(|error| error.to_string())?;
        let mut file =
            lingxi_core::host::rooted_fs::open_read_file_pinned(root, relative, Some(&identity))
                .map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((bytes, identity))
    })
    .await
    .map_err(|error| fail(error.to_string()))?
    .map_err(fail)?;
    let source = committed_mobile_source(&raw_source).map_err(fail)?;
    let loaded = session::jsonl::loader::route_captured_session_source(&path, id, &source)
        .map_err(|error| fail(format!("invalid captured transcript: {error}")))?;
    if loaded.malformed_line_count != 0 {
        return Err(fail("transcript contains malformed records".into()));
    }
    let profile = session::jsonl::JsonlWriter::fixed_session_profile_from_source(&source, &id_text)
        .map_err(|error| fail(format!("invalid conversation mode: {error}")))?;
    let mode = profile.mode;
    if mode != cfg.session_mode {
        return Err(fail(format!(
            "session mode mismatch: expected {}, found {}",
            cfg.session_mode.as_str(),
            mode.as_str()
        )));
    }
    let recorded = session::jsonl::loader::transcript_workspace_path(&path, &loaded)
        .ok_or_else(|| fail("transcript has no valid workspace".into()))?;
    let requested_cwd = Path::new(&recorded);
    let host = model_fs
        .translate_model_path(&recorded, false)
        .map_err(|error| fail(format!("invalid workspace: {error}")))?
        .map(PathBuf::from)
        .unwrap_or_else(|| requested_cwd.to_path_buf())
        .canonicalize()
        .map_err(|error| fail(format!("workspace unavailable: {error}")))?;
    if !host.is_dir() {
        return Err(fail("workspace is not a directory".into()));
    }
    let logical = if let Some(runtime) = platform.mobile_linux() {
        let mounts = runtime.current_mounts();
        let guest = mobile_linux_api::map_host_path_to_guest(&host, &mounts)
            .map(PathBuf::from)
            .ok_or_else(|| fail("workspace is outside mounted storage".into()))?;
        if !guest.starts_with(mobile_linux_api::guest_paths::HOME) {
            return Err(fail("workspace is outside the Linux user's home".into()));
        }
        guest
    } else {
        host.clone()
    };
    let project_root = loaded
        .mobile_empty_session_cwds
        .get(&id_text)
        .map(PathBuf::from)
        .or_else(|| {
            loaded
                .messages_in_order
                .first()
                .map(|row| PathBuf::from(&row.cwd))
        })
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| logical.clone());
    let permission_mode = loaded.permission_modes.get(&id_text).cloned();
    let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &id_text);
    let replayed = if loaded.messages_in_order.is_empty()
        && loaded.mobile_empty_sessions.contains(&id_text)
        || super::mobile_anchored_startup_only(&loaded, id)
    {
        super::replay_mobile_anchored_startup(
            path,
            source.clone(),
            loaded,
            id,
            profile.instruction_epoch,
        )
    } else {
        orchestrator::resume::replay_captured_session(path, source.clone(), loaded, id)
            .map_err(|error| fail(format!("cannot replay transcript: {error}")))?
    };
    Ok(Some(InitialMobileSession {
        id,
        source,
        raw_source,
        transcript_identity,
        mode,
        messages,
        replayed,
        permission_mode,
        project_root,
        host_cwd: host,
        logical_cwd: logical,
    }))
}

pub(super) fn lower_session_mode(mode: session::jsonl::SessionMode) -> SessionModeDto {
    match mode {
        session::jsonl::SessionMode::Chat => SessionModeDto::Chat,
        session::jsonl::SessionMode::Code => SessionModeDto::Code,
    }
}

/// ONE canonical spelling for a session-catalog cwd key. `canonicalize`
/// collapses the platform's symlink split (`/var` vs `/private/var` on
/// iOS/macOS), so listing, resume and the cwd gates all derive the SAME
/// sanitized `projects/` directory.
///
/// A bare `canonicalize(path).unwrap_or(raw)` disagrees with itself across a
/// path's lifetime: while the directory exists it spells `/private/var/...`, but
/// once it is gone `canonicalize` fails and the raw fallback spells a different
/// catalog directory. Walk up to the nearest surviving ancestor, canonicalize
/// THAT, and reappend the removed suffix, so both calls agree.
pub(crate) fn canonical_cwd_string(path: &std::path::Path) -> String {
    let mut removed_suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                for name in removed_suffix.into_iter().rev() {
                    canonical.push(name);
                }
                return canonical.to_string_lossy().to_string();
            }
            Err(_) => match ancestor.file_name().map(std::ffi::OsString::from) {
                Some(name) => {
                    removed_suffix.push(name);
                    if !ancestor.pop() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    path.to_string_lossy().to_string()
}

/// Recover only an incomplete final record. A valid unterminated record gets
/// its delimiter, while malformed complete/interior records remain errors.
fn committed_mobile_source(bytes: &[u8]) -> Result<String, String> {
    let mut end = bytes.len();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let last = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let valid = std::str::from_utf8(&bytes[last..])
            .ok()
            .is_some_and(|line| {
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line).is_ok()
            });
        if !valid {
            end = last;
        }
    }
    let mut source = String::from_utf8(bytes[..end].to_vec())
        .map_err(|_| "transcript contains invalid UTF-8 before its final fragment".to_string())?;
    if !source.is_empty() && !source.ends_with('\n') {
        source.push('\n');
    }
    Ok(source)
}

#[cfg(test)]
mod committed_source_tests {
    use super::committed_mobile_source;

    #[test]
    fn only_a_torn_suffix_is_discarded_and_valid_unterminated_rows_are_delimited() {
        let prefix = b"{\"type\":\"user\"}\n";
        let mut torn = prefix.to_vec();
        torn.extend_from_slice(b"{\"type\":\"assistant\",\"text\":\"\xf0\x9f");
        assert_eq!(committed_mobile_source(&torn).unwrap().as_bytes(), prefix);
        assert_eq!(
            committed_mobile_source(b"{\"type\":\"user\"}")
                .unwrap()
                .as_bytes(),
            prefix
        );
        assert!(committed_mobile_source(b"\xff\n{\"torn\"").is_err());
        let interior = committed_mobile_source(b"{bad}\n{\"type\":\"user\"}\n").unwrap();
        assert_ne!(
            session::jsonl::route_lines(&interior).malformed_line_count,
            0
        );
    }
}
