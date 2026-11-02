//! Validation shared by scheduled session anchor owners.

/// An unprompted desktop draft may contain profile metadata and host lifecycle
/// hook attachments. Reject malformed, message-bearing, foreign-session or
/// unrecognized rows rather than treating a damaged conversation as a draft.
#[must_use]
pub fn is_mode_only_draft(source: &str, session_id: &str) -> bool {
    if source.is_empty() || !source.ends_with('\n') {
        return false;
    }
    if crate::JsonlWriter::fixed_session_mode_from_source(source, session_id).is_err() {
        return false;
    }
    let Ok(target_id) = uuid::Uuid::parse_str(session_id) else {
        return false;
    };
    let mut hook_rows = std::collections::HashSet::new();
    let mut saw_row = false;
    let valid_rows = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| {
            let Ok(row) = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line) else {
                return false;
            };
            if !row
                .string_at("/sessionId")
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .is_some_and(|value| value == target_id)
            {
                return false;
            }
            saw_row = true;
            // These fields can carry user, assistant or tool conversation even if
            // the row's outer type was changed or its message chain is damaged.
            if ["message", "rendered", "renderedRole", "toolUseResult"]
                .iter()
                .any(|field| row.contains_property(field))
            {
                return false;
            }
            if row.string_at("/type") == Some("attachment") {
                let Some(id) = row
                    .string_at("/uuid")
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                else {
                    return false;
                };
                let parent_valid = row.at_pointer("/parentUuid").is_some_and(|parent| {
                    parent.is_null()
                        || parent
                            .as_str()
                            .and_then(|id| uuid::Uuid::parse_str(id).ok())
                            .is_some_and(|id| hook_rows.contains(&id))
                });
                if !parent_valid
                    || hook_rows.contains(&id)
                    || row
                        .at_pointer("/isSidechain")
                        .and_then(|value| value.as_bool())
                        != Some(false)
                    || row.string_at("/timestamp").is_none_or(str::is_empty)
                    || row.string_at("/cwd").is_none_or(str::is_empty)
                    || row.string_at("/version").is_none_or(str::is_empty)
                    || !is_draft_lifecycle_hook_attachment(&row)
                {
                    return false;
                }
                hook_rows.insert(id);
                return true;
            }
            if row.contains_property("attachment") {
                return false;
            }
            if row.string_at("/type") == Some("fixed-session-mode") {
                return true;
            }
            is_draft_metadata_row(&row)
        });
    valid_rows && saw_row
}

/// Only recognize the known, typed metadata records emitted for a draft.
fn is_draft_metadata_row(row: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
    let text = |pointer: &str| row.string_at(pointer).is_some();
    let text_or_null = |pointer: &str| {
        row.at_pointer(pointer)
            .is_some_and(|value| value.is_null() || value.as_str().is_some())
    };
    let object_or_null = |pointer: &str| {
        row.at_pointer(pointer)
            .is_some_and(|value| value.is_null() || value.object_entries().is_some())
    };
    match row.string_at("/type") {
        Some("permission-mode") => text("/permissionMode"),
        Some("mode") => text("/mode"),
        Some("custom-title") => text_or_null("/customTitle"),
        Some("ai-title") => text_or_null("/aiTitle"),
        Some("tag") => text_or_null("/tag"),
        Some("relocated") => row
            .string_at("/relocatedCwd")
            .is_some_and(|value| !value.is_empty()),
        Some("agent-name") => text("/agentName"),
        Some("agent-color") => text("/agentColor"),
        Some("agent-setting") => text("/agentSetting") || object_or_null("/agentSetting"),
        Some("isolation-latch") => text("/side"),
        Some("atis-latch") => text("/atis"),
        Some("worktree-state") => object_or_null("/worktreeSession"),
        Some("pr-link") => {
            row.at_pointer("/prNumber")
                .and_then(|value| value.as_number())
                .is_some()
                && text("/prUrl")
                && text("/prRepository")
        }
        Some("bridge-session") => {
            text("/bridgeSessionId")
                && row
                    .at_pointer("/lastSequenceNum")
                    .and_then(|value| value.as_number())
                    .is_some()
        }
        _ => false,
    }
}

/// Accept only the host lifecycle hook records that can precede a first turn.
/// In particular, never admit deferred tools or prompt/tool hook attachments.
fn is_draft_lifecycle_hook_attachment(
    row: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> bool {
    let event = row.string_at("/attachment/hookEvent");
    let name = row.string_at("/attachment/hookName");
    let kind = row.string_at("/attachment/type");
    if kind == Some("hook_additional_context") {
        let content = row.at_pointer("/attachment/content");
        return event == Some("SessionStart")
            && name == Some("SessionStart")
            && row.string_at("/attachment/toolUseID") == Some("SessionStart")
            && content
                .and_then(|value| value.array_len())
                .is_some_and(|length| {
                    length > 0
                        && (0..length).all(|index| {
                            content
                                .and_then(|value| value.get_index(index))
                                .and_then(|value| value.as_str())
                                .is_some()
                        })
                });
    }
    let valid_event = match event {
        Some("SessionStart") => {
            matches!(name, Some("SessionStart:startup" | "SessionStart:resume"))
        }
        Some(
            "SessionEnd" | "InstructionsLoaded" | "PreModelSwitch" | "PostModelSwitch"
            | "ConfigChange" | "FileChanged",
        ) => name == event,
        _ => false,
    };
    if !valid_event
        || row
            .string_at("/attachment/toolUseID")
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .is_none()
    {
        return false;
    }
    let string = |field: &str| row.string_at(field).is_some();
    let number = |field: &str| {
        row.at_pointer(field)
            .and_then(|value| value.as_number())
            .is_some()
    };
    match kind {
        Some("hook_success") => {
            string("/attachment/content")
                && string("/attachment/stdout")
                && string("/attachment/stderr")
                && number("/attachment/exitCode")
                && string("/attachment/command")
                && number("/attachment/durationMs")
        }
        Some("hook_non_blocking_error") => {
            string("/attachment/stdout")
                && string("/attachment/stderr")
                && number("/attachment/exitCode")
        }
        Some("hook_cancelled") => true,
        Some("hook_system_message") => string("/attachment/content"),
        _ => false,
    }
}

/// Validate and sync one already-published target while both writer locks are held.
pub(crate) fn ensure_anchor_at(
    path: &std::path::Path,
    cwd: &std::path::Path,
    session_id: lingxi_core::types::SessionId,
    mode: lingxi_core::host::session_context::ConversationMode,
) -> Result<bool, super::TranscriptWriterError> {
    use lingxi_core::host::rooted_fs::{
        open_existing_file_for_update_pinned, open_read_file_pinned, opened_file_identity,
        root_identity, sync_parent_pinned, validate_open_file_single_link,
    };
    use lingxi_core::host::FsError;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use std::io::{Read, Seek, SeekFrom, Write};

    let invalid = |message: &str| FsError::Io(message.to_owned());
    let uuid = session_id.as_uuid();
    let session = uuid.to_string();
    let cwd = cwd
        .to_str()
        .filter(|_| cwd.is_absolute())
        .ok_or_else(|| invalid("invalid scheduled session cwd"))?;
    if path.extension().and_then(|part| part.to_str()) != Some("jsonl")
        || path
            .file_stem()
            .and_then(|part| part.to_str())
            .and_then(|part| uuid::Uuid::parse_str(part).ok())
            != Some(uuid)
    {
        return Err(invalid("scheduled transcript does not belong to its session").into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("scheduled transcript has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("scheduled transcript has no filename"))?;
    let relative = std::path::Path::new(name);
    let root = root_identity(parent)?;
    let mut file = open_existing_file_for_update_pinned(parent, relative, &root)?;
    validate_open_file_single_link(&file)?;
    let identity = opened_file_identity(&file)?;
    let mut source = String::new();
    file.read_to_string(&mut source)
        .map_err(|error| invalid(&error.to_string()))?;
    if file
        .metadata()
        .map_err(|error| invalid(&error.to_string()))?
        .len()
        != source.len() as u64
        || !source.ends_with('\n')
        || crate::JsonlWriter::fixed_session_mode_from_source(&source, &session)
            .map_err(|error| invalid(&error))?
            != mode
    {
        return Err(invalid("scheduled session mode or transcript changed").into());
    }
    let mut recorded_cwd: Option<String> = None;
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let row = Utf16JsonProjection::parse(line)
            .map_err(|_| invalid("invalid scheduled transcript row"))?;
        if row.contains_property("sessionId")
            && row
                .string_at("/sessionId")
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                != Some(uuid)
        {
            return Err(invalid("scheduled transcript has a different session UUID").into());
        }
        if row.contains_property("cwd") {
            let row_cwd = row
                .string_at("/cwd")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid("scheduled transcript has an invalid cwd"))?;
            if recorded_cwd
                .as_deref()
                .is_some_and(|previous| previous != row_cwd)
            {
                return Err(invalid("scheduled transcript cwd changed without relocation").into());
            }
            recorded_cwd = Some(row_cwd.to_owned());
        }
        if row.contains_property("mobileEmptySession")
            && (row.string_at("/type") != Some("custom-title")
                || row.string_at("/customTitle").is_none()
                || row.number_at("/mobileEmptySession") != Some(1.0)
                || row
                    .string_at("/sessionId")
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    != Some(uuid)
                || row.string_at("/cwd").is_none())
        {
            return Err(invalid("invalid scheduled empty-session anchor").into());
        }
        if row.string_at("/type") == Some("relocated") {
            recorded_cwd = Some(
                row.string_at("/relocatedCwd")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| invalid("scheduled relocation has no cwd"))?
                    .to_owned(),
            );
        }
    }
    if recorded_cwd.as_deref().is_some_and(|last| last != cwd) {
        return Err(invalid("scheduled transcript has a different current cwd").into());
    }
    let loaded = super::route_lines(&source);
    if loaded.malformed_line_count != 0 {
        return Err(invalid("scheduled transcript is malformed").into());
    }
    let has_conversation = loaded
        .messages_in_order
        .iter()
        .any(|message| matches!(message.message_type.as_str(), "user" | "assistant"));
    if !has_conversation && !is_mode_only_draft(&source, &session) {
        return Err(invalid("scheduled transcript is not an empty draft").into());
    }
    let added = !has_conversation && !loaded.mobile_empty_sessions.contains(&session);
    if added {
        validate_open_file_single_link(&file)?;
        let title = loaded
            .custom_titles
            .get(&session)
            .filter(|title| !title.is_empty())
            .map_or("Scheduled task", String::as_str);
        let line = format!(
            "{}\n",
            serde_json::json!({
                "type": "custom-title",
                "customTitle": title,
                "sessionId": session,
                "mobileEmptySession": 1,
                "cwd": cwd,
            })
        );
        file.seek(SeekFrom::End(0))
            .map_err(|error| invalid(&error.to_string()))?;
        if let Err(error) = file.write_all(line.as_bytes()) {
            file.set_len(source.len() as u64)
                .and_then(|()| file.sync_all())
                .map_err(|rollback| {
                    invalid(&format!("{error}; anchor rollback failed: {rollback}"))
                })?;
            return Err(invalid(&error.to_string()).into());
        }
    }
    let sync = file
        .sync_all()
        .map_err(|error| invalid(&error.to_string()))
        .and_then(|()| sync_parent_pinned(parent, relative, Some(&root)));
    if let Err(error) = sync {
        return if added {
            Err(super::TranscriptWriterError::WrittenButNotDurable(error))
        } else {
            Err(error.into())
        };
    }
    let current = open_read_file_pinned(parent, relative, Some(&root))?;
    if opened_file_identity(&current)? != identity || validate_open_file_single_link(&file).is_err()
    {
        return Err(invalid("scheduled transcript changed during anchor creation").into());
    }
    Ok(added)
}
