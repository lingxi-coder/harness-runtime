use client::protocol::listings::SessionModeDto;

/// A single validated capture used throughout a cold mobile mount. The selected
/// path is validated against the catalog; the workspace and immutable profile
/// come from that same captured record.
pub(super) struct InitialMobileSession {
    pub id: uuid::Uuid,
    pub selected_path: std::path::PathBuf,
    pub source: String,
    pub raw_source: Vec<u8>,
    pub transcript_root: std::path::PathBuf,
    pub transcript_relative: std::path::PathBuf,
    pub transcript_identity: lingxi_core::host::rooted_fs::RootIdentity,
    pub transcript_file_identity: lingxi_core::host::rooted_fs::RootIdentity,
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

    let fail = |reason: String| {
        super::MobileBuildError::Orchestrator(format!("initial session resume: {reason}"))
    };
    let (requested, selected) = match (
        cfg.initial_session_id.as_deref(),
        cfg.initial_session_transcript_path.as_deref(),
    ) {
        (None, None) => return Ok(None),
        (Some(id), Some(path)) => (id, path),
        _ => {
            return Err(fail(
                "session ID and selected transcript path are both required".into(),
            ))
        }
    };
    let selected_path = selected.to_path_buf();
    if cfg.app_agent_host {
        return Err(fail("Local App Agent cannot mount a conversation".into()));
    }
    let id = uuid::Uuid::parse_str(requested.strip_prefix("sess:").unwrap_or(requested))
        .map_err(|error| fail(format!("invalid session UUID: {error}")))?;
    let id_text = id.to_string();
    let model_fs = platform.filesystem();
    let home = cfg
        .lingxi_home
        .canonicalize()
        .map_err(|error| fail(format!("invalid session home: {error}")))?;
    let catalog = home.join("projects");
    // Catalog results may be in native coordinates or the mobile guest.
    // Only canonicalize the trusted parent of projects; never resolve the
    // selected bucket or file before the rooted no-follow traversal.
    let native_in_catalog = selected
        .parent()
        .and_then(Path::parent)
        .and_then(|parent| parent.parent())
        .and_then(|parent| parent.canonicalize().ok())
        .is_some_and(|parent| parent == home);
    let selected = if native_in_catalog {
        selected.to_path_buf()
    } else {
        model_fs
            .translate_model_path(&selected.to_string_lossy(), false)
            .map_err(|error| fail(format!("invalid session source: {error}")))?
            .map(PathBuf::from)
            .unwrap_or_else(|| selected.to_path_buf())
    };
    let bucket = selected
        .parent()
        .ok_or_else(|| fail("transcript has no bucket".into()))?;
    let selected_catalog = bucket
        .parent()
        .ok_or_else(|| fail("transcript has no catalog".into()))?;
    let metadata = std::fs::symlink_metadata(selected_catalog)
        .map_err(|error| fail(format!("session catalog unavailable: {error}")))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || selected_catalog.canonicalize().ok().as_deref() != Some(catalog.as_path())
    {
        return Err(fail("session source is outside a real catalog".into()));
    }
    let bucket_name = bucket
        .file_name()
        .ok_or_else(|| fail("transcript has no bucket name".into()))?;
    let name = selected
        .file_name()
        .ok_or_else(|| fail("transcript has no filename".into()))?;
    let transcript_relative = Path::new("projects").join(bucket_name).join(name);
    let path = home.join(&transcript_relative);
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| session::jsonl::validate_uuid(value))
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            != Some(id)
    {
        return Err(fail(
            "selected transcript does not match the session UUID".into(),
        ));
    }
    let transcript_root = home.clone();
    let capture_root = home;
    let capture_relative = transcript_relative.clone();
    let (raw_source, transcript_identity, transcript_file_identity) =
        tokio::task::spawn_blocking(move || {
            use lingxi_core::host::rooted_fs::{
                open_read_file_pinned, opened_file_identity, root_identity,
                validate_open_file_single_link,
            };
            use std::io::Read;
            let identity = root_identity(&capture_root).map_err(|error| error.to_string())?;
            let mut file = open_read_file_pinned(&capture_root, &capture_relative, Some(&identity))
                .map_err(|error| error.to_string())?;
            validate_open_file_single_link(&file).map_err(|error| error.to_string())?;
            let file_identity = opened_file_identity(&file).map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            let current = open_read_file_pinned(&capture_root, &capture_relative, Some(&identity))
                .map_err(|error| error.to_string())?;
            if opened_file_identity(&current).map_err(|error| error.to_string())? != file_identity {
                return Err("session source changed during capture".into());
            }
            Ok::<_, String>((bytes, identity, file_identity))
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
    let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &id_text);
    if messages.is_empty() {
        let anchored = session::jsonl::validated_empty_draft_cwd(&source, &id_text)
            .ok_or_else(|| fail("empty session anchor is invalid".into()))?;
        if anchored.as_path() != Path::new(&recorded) {
            return Err(fail("empty session anchor workspace does not match".into()));
        }
    }
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
        let mut mounts = runtime.current_mounts();
        // The selected workspace was canonicalized above. Normalize the
        // trusted mount roots the same way before the lexical reverse map,
        // including hosts where /var and /private/var name the same tree.
        for mount in &mut mounts {
            mount.host_path = PathBuf::from(super::canonical_cwd_string(&mount.host_path));
        }
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
        selected_path,
        source,
        raw_source,
        transcript_root,
        transcript_relative,
        transcript_identity,
        transcript_file_identity,
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
