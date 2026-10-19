//! Rebuild only file snapshots proven by successful, model-visible tool history.
//!
//! Transcript rows do not carry the original file mtime. A recovered snapshot
//! therefore uses the oldest timestamp, forcing the existing write guard to
//! compare its content with the current file even if the mtime went backwards.
//! This replay never reads the filesystem or treats present-day bytes as read.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use lingxi_core::SessionState;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tool_api::read_file_state::{ReadFileEntry, ReadFileStateMap};

#[derive(Clone)]
struct FileCall {
    name: String,
    path: PathBuf,
    input: Value,
}

fn projection_value(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Option<Value> {
    // Reject strings that cannot be represented exactly as UTF-8, rather than
    // allowing a lossy projection to become a trusted file snapshot.
    serde_json::from_str(&projection.to_json_string().ok()?).ok()
}

fn file_call(block: &ContentBlock, cwd: &Path) -> Option<(ToolUseId, FileCall)> {
    let ContentBlock::ToolUse {
        id, name, input, ..
    } = block
    else {
        return None;
    };
    if !matches!(name.as_str(), "Read" | "Write" | "Edit" | "NotebookEdit") {
        return None;
    }
    let input = projection_value(input)?;
    let raw_path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))?
        .as_str()?;
    // A transcript can span cwd/worktree changes but lacks a cwd per tool row.
    // Only absolute model routes are unambiguous during historical replay.
    if !Path::new(raw_path).is_absolute() {
        return None;
    }
    Some((
        id.clone(),
        FileCall {
            name: name.clone(),
            path: tool_file::normalize_model_file_path(raw_path, cwd),
            input,
        },
    ))
}

fn message_blocks(message: &ConversationMessage) -> &[ContentBlock] {
    match message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content,
        ConversationMessage::System { .. } => &[],
    }
}

/// Populate a caller-cleared registry from the active model history. The caller
/// owns session switching and must discard entries belonging to the old session.
pub(super) fn restore_file_state(
    session: &SessionState,
    cwd: &Path,
    map: &ReadFileStateMap,
    attachments: &HashMap<MessageId, Utf16JsonProjection>,
) {
    let history = session.model_context_history();
    let mut calls: HashMap<ToolUseId, Vec<PathBuf>> = HashMap::new();
    let mut call_counts: HashMap<ToolUseId, usize> = HashMap::new();
    let mut result_counts: HashMap<ToolUseId, usize> = HashMap::new();
    for message in &history {
        for block in message_blocks(message) {
            match (message, block) {
                (ConversationMessage::Assistant { .. }, ContentBlock::ToolUse { id, .. }) => {
                    *call_counts.entry(id.clone()).or_default() += 1;
                    if let Some((id, call)) = file_call(block, cwd) {
                        calls.entry(id).or_default().push(call.path);
                    }
                }
                (
                    ConversationMessage::User { .. },
                    ContentBlock::ToolResult { tool_use_id, .. },
                ) => {
                    *result_counts.entry(tool_use_id.clone()).or_default() += 1;
                }
                _ => {}
            }
        }
    }

    let mut pending = HashMap::new();
    let mut cache = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for message in &history {
        if let Some((path, entry)) = attachment_snapshot(message, attachments, cwd) {
            if let Some(entry) = entry {
                cache.set(path, entry);
            } else {
                cache.remove(&path);
            }
        }
        match message {
            ConversationMessage::Assistant { content, .. } => {
                for block in content {
                    if let Some((id, call)) = file_call(block, cwd) {
                        pending.insert(id, call);
                    }
                }
            }
            ConversationMessage::User { id, content, .. } => {
                let result_count = content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
                    .count();
                let raw = session
                    .native_request_history
                    .task_delivery_envelopes
                    .get(id)
                    .and_then(|envelope| envelope.tool_use_result.as_ref());
                for block in content {
                    let ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } = block
                    else {
                        continue;
                    };
                    let Some(call) = pending.remove(tool_use_id) else {
                        continue;
                    };
                    if *is_error == Some(true) {
                        continue;
                    }
                    // A row has one raw sidecar. Never attach that sidecar to
                    // multiple results, reused tool ids, or an out-of-order call.
                    if result_count != 1
                        || call_counts.get(tool_use_id) != Some(&1)
                        || result_counts.get(tool_use_id) != Some(&1)
                    {
                        if let Some(ambiguous_calls) = calls.get(tool_use_id) {
                            for path in ambiguous_calls {
                                cache.remove(path);
                            }
                        }
                        continue;
                    }
                    let data = raw.and_then(projection_value);
                    let prior = cache.peek(&call.path);
                    let snapshot = match data.as_ref() {
                        Some(data) => recover_snapshot(&call, data, prior.as_ref(), cwd),
                        // Writer-less in-memory histories have no raw envelope.
                        // A successful Write still proves its exact input bytes.
                        None if raw.is_none() && call.name == "Write" => call
                            .input
                            .get("content")
                            .and_then(Value::as_str)
                            .and_then(write_snapshot),
                        _ => None,
                    };
                    if let Some(entry) = snapshot {
                        cache.set(call.path, entry);
                    } else {
                        // A later successful unsupported or malformed operation
                        // must not leave an older snapshot appearing current.
                        cache.remove(&call.path);
                    }
                }
            }
            ConversationMessage::System { .. } => {}
        }
    }
}

fn attachment_snapshot(
    message: &ConversationMessage,
    attachments: &HashMap<MessageId, Utf16JsonProjection>,
    cwd: &Path,
) -> Option<(PathBuf, Option<ReadFileEntry>)> {
    let attachment = attachments.get(&message.id())?;
    if attachment.string_at("/type")? != "file"
        || crate::conversation::ConversationOrchestrator::file_attachment_projection(
            message.id(),
            attachment,
        )
        .as_ref()
            != Some(message)
    {
        return None;
    }
    let data = projection_value(attachment)?;
    let filename = data.get("filename")?.as_str()?;
    if !Path::new(filename).is_absolute() {
        return None;
    }
    let path = tool_file::normalize_model_file_path(filename, cwd);
    let entry = (|| {
        let file = data.pointer("/content/file")?;
        let result_path = file.get("filePath")?.as_str()?;
        if !Path::new(result_path).is_absolute()
            || tool_file::normalize_model_file_path(result_path, cwd) != path
            || file
                .get("truncatedByTokenCap")
                .is_some_and(|value| value != &Value::Bool(false))
        {
            return None;
        }
        let content = file.get("content")?.as_str()?;
        // Post-compact file attachments count the final empty fragment even
        // for empty files, unlike ordinary Read's numLines/empty-file fields.
        let lines = content.bytes().filter(|byte| *byte == b'\n').count() as u64 + 1;
        if file.get("startLine")?.as_u64()? != 1
            || file.get("numLines")?.as_u64()? != lines
            || file.get("totalLines")?.as_u64()? != lines
        {
            return None;
        }
        Some(snapshot(content.to_owned(), true))
    })();
    Some((path, entry))
}

fn snapshot(content: String, from_read: bool) -> ReadFileEntry {
    ReadFileEntry {
        content,
        mtime_ms: i64::MIN,
        offset: None,
        limit: None,
        from_read,
        seeded_from_context: false,
        is_partial_view: false,
    }
}

fn write_snapshot(content: &str) -> Option<ReadFileEntry> {
    // The guard compares the same BOM-aware decoded form as Read, while Write
    // records the exact input bytes (which may include a leading UTF-8 BOM).
    Some(snapshot(
        tool_file::shared::decode_utf8_strict(content.as_bytes()).ok()?,
        false,
    ))
}

fn recover_snapshot(
    call: &FileCall,
    data: &Value,
    prior: Option<&ReadFileEntry>,
    cwd: &Path,
) -> Option<ReadFileEntry> {
    let result_path = if call.name == "Read" {
        data.pointer("/file/filePath")
    } else {
        data.get("filePath")
    }?
    .as_str()?;
    if !Path::new(result_path).is_absolute()
        || tool_file::normalize_model_file_path(result_path, cwd) != call.path
    {
        return None;
    }
    match call.name.as_str() {
        "Read" if data.get("type")?.as_str()? == "file_unchanged" => prior.cloned(),
        "Read" => {
            if data.get("type")?.as_str()? != "text" {
                return None;
            }
            let file = data.get("file")?;
            if file
                .get("truncatedByTokenCap")
                .is_some_and(|value| value != &Value::Bool(false))
            {
                return None;
            }
            let content = file.get("content")?.as_str()?;
            let start = file.get("startLine")?.as_u64()?;
            let count = file.get("numLines")?.as_u64()?;
            let total = file.get("totalLines")?.as_u64()?;
            if count != content.split_inclusive('\n').count() as u64 || start == 0 {
                return None;
            }
            // A capped prefix ending in '\n' has the same apparent total as a
            // whole trailing-newline file: Read(limit=1) of "one\ntwo" returns
            // "one\n", numLines=1,totalLines=2. Require evidence of reaching
            // EOF as well as matching the source's phantom-final-line count.
            let reached_eof = match call.input.get("limit") {
                None | Some(Value::Null) => true,
                Some(limit) => limit
                    .as_u64()
                    .is_some_and(|limit| count < limit || (count == limit && count == total)),
            };
            if start == 1 && total == total_lines(content) && reached_eof {
                return Some(snapshot(content.to_owned(), true));
            }
            // Ranged reads cannot manufacture unseen bytes. Retain a previous
            // full snapshot only if both the slice and whole-file size agree.
            let prior = prior?;
            let first = usize::try_from(start.checked_sub(1)?).ok()?;
            let count = usize::try_from(count).ok()?;
            let lines: Vec<_> = prior.content.split_inclusive('\n').collect();
            if total != total_lines(&prior.content)
                || lines.get(first..first.checked_add(count)?)?.concat() != content
            {
                return None;
            }
            Some(prior.clone())
        }
        "Write" => {
            if !matches!(data.get("type")?.as_str()?, "create" | "update") {
                return None;
            }
            write_snapshot(data.get("content")?.as_str()?)
        }
        "Edit" => {
            let before = data.get("originalFile")?.as_str()?;
            let mut after = apply_structured_patch(before, data.get("structuredPatch")?)?;
            // Edit's patch is LF-normalized, while its actual write preserves
            // the dominant line ending. Use its real codec with a proven prior
            // snapshot; no current disk read may supply historical metadata.
            if let Some(prior) = prior {
                let (normalized, encoding, ending) =
                    tool_file::file_meta::read_with_metadata(prior.content.as_bytes());
                if normalized == before
                    || normalized == before.strip_prefix('\u{feff}').unwrap_or(before)
                {
                    let bytes =
                        tool_file::file_meta::encode_with_metadata(&after, encoding, ending);
                    after = String::from_utf8(bytes).ok()?;
                }
            }
            write_snapshot(&after)
        }
        _ => None,
    }
}

fn total_lines(content: &str) -> u64 {
    if content.is_empty() {
        0
    } else {
        content.bytes().filter(|byte| *byte == b'\n').count() as u64 + 1
    }
}

/// Apply jsdiff's complete hunk array without fuzzy matching or oldString
/// replacement. The latter can differ from the tool's quote-adapted edit.
fn apply_structured_patch(before: &str, patch: &Value) -> Option<String> {
    let original: Vec<_> = before.split_inclusive('\n').collect();
    let mut output = Vec::new();
    let mut cursor = 0;
    for hunk in patch.as_array()? {
        let old_start = usize::try_from(hunk.get("oldStart")?.as_u64()?)
            .ok()?
            .checked_sub(1)?;
        let new_start = usize::try_from(hunk.get("newStart")?.as_u64()?)
            .ok()?
            .checked_sub(1)?;
        let old_count = usize::try_from(hunk.get("oldLines")?.as_u64()?).ok()?;
        let new_count = usize::try_from(hunk.get("newLines")?.as_u64()?).ok()?;
        output.extend(
            original
                .get(cursor..old_start)?
                .iter()
                .map(|line| (*line).to_owned()),
        );
        if new_start != output.len() {
            return None;
        }
        cursor = old_start;
        let output_start = output.len();
        let lines = hunk.get("lines")?.as_array()?;
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index].as_str()?;
            let (prefix, body) = line.split_at_checked(1)?;
            if !matches!(prefix, " " | "-" | "+") || body.contains('\n') {
                return None;
            }
            let mut text = body.to_owned();
            if lines.get(index + 1).and_then(Value::as_str) == Some("\\ No newline at end of file")
            {
                index += 1;
            } else {
                text.push('\n');
            }
            if prefix != "+" {
                if original.get(cursor).copied()? != text {
                    return None;
                }
                cursor += 1;
            }
            if prefix != "-" {
                output.push(text);
            }
            index += 1;
        }
        if cursor.checked_sub(old_start)? != old_count
            || output.len().checked_sub(output_start)? != new_count
        {
            return None;
        }
    }
    output.extend(
        original
            .get(cursor..)?
            .iter()
            .map(|line| (*line).to_owned()),
    );
    if output
        .iter()
        .take(output.len().saturating_sub(1))
        .any(|line| !line.ends_with('\n'))
    {
        return None;
    }
    Some(output.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::TaskDeliveryHistoryEnvelope;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{MessageId, SessionId};
    use serde_json::json;
    use tool_api::read_file_state::{get, new_read_file_state_map};

    const FILE: &str = "/workspace/file.txt";

    fn projection(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(&value.to_string()).unwrap()
    }

    fn session() -> SessionState {
        SessionState::empty(SessionId::new(), "test-model".to_owned())
    }

    fn call(session: &mut SessionState, name: &str, input: Value) -> ToolUseId {
        let id = ToolUseId::new();
        session.history.push(ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: name.to_owned(),
                input: projection(input),
                provider_id: None,
            }],
            stop_reason: None,
            resumable: None,
            per_turn_effort: None,
        });
        id
    }

    fn result(
        session: &mut SessionState,
        tool_use_id: ToolUseId,
        data: Option<Value>,
        error: bool,
    ) -> MessageId {
        let id = MessageId::new();
        let mut message = ConversationMessage::user(id, String::new());
        if let ConversationMessage::User { content, .. } = &mut message {
            content.push(ContentBlock::ToolResult {
                tool_use_id,
                content: projection(json!("tool result")),
                is_error: Some(error),
                provider_tool_use_id: None,
                content_blocks: None,
            });
        }
        session.history.push(message);
        if let Some(data) = data {
            session
                .native_request_history
                .task_delivery_envelopes
                .insert(
                    id,
                    TaskDeliveryHistoryEnvelope {
                        tool_use_result: Some(projection(data)),
                        ..Default::default()
                    },
                );
        }
        id
    }

    fn read_data(content: &str) -> Value {
        json!({"type":"text","file":{
            "filePath":FILE,"content":content,"startLine":1,
            "numLines":content.split_inclusive('\n').count(),"totalLines":total_lines(content)
        }})
    }

    fn write_data(content: &str) -> Value {
        json!({"type":"update","filePath":FILE,"content":content})
    }

    fn append_read(session: &mut SessionState, content: &str) -> MessageId {
        let id = call(session, "Read", json!({"file_path":FILE}));
        result(session, id, Some(read_data(content)), false)
    }

    fn append_write(session: &mut SessionState, content: &str) -> MessageId {
        let id = call(
            session,
            "Write",
            json!({"file_path":FILE,"content":content}),
        );
        result(session, id, Some(write_data(content)), false)
    }

    fn restored(session: &SessionState) -> Option<ReadFileEntry> {
        let map = new_read_file_state_map();
        restore_file_state(session, Path::new("/workspace"), &map, &HashMap::new());
        get(&map, Path::new(FILE))
    }

    #[test]
    fn full_reads_and_writes_preserve_exact_text_and_force_current_content_check() {
        for content in ["", "one", "one\n", "\n", "one\r\ntwo\r\n", "one\r\ntwo"] {
            for read in [true, false] {
                let mut session = session();
                if read {
                    append_read(&mut session, content);
                } else {
                    append_write(&mut session, content);
                }
                let entry = restored(&session).unwrap();
                assert_eq!(entry.content, content);
                assert_eq!(entry.mtime_ms, i64::MIN);
                assert_eq!(entry.from_read, read);
                assert_eq!(entry.offset, None);
                assert_eq!(entry.limit, None);
                assert!(!entry.is_partial_view);
            }
        }
    }

    #[test]
    fn successful_write_without_sidecar_uses_input_but_failed_or_malformed_never_does() {
        for (sidecar, error, expected) in [
            (None, false, Some("new")),
            (None, true, Some("old")),
            (Some(Value::Null), false, None),
            (
                Some(json!({"type":"update","filePath":"/other","content":"new"})),
                false,
                None,
            ),
        ] {
            let mut session = session();
            append_read(&mut session, "old");
            let id = call(
                &mut session,
                "Write",
                json!({"file_path":FILE,"content":"new"}),
            );
            result(&mut session, id, sidecar, error);
            assert_eq!(
                restored(&session)
                    .as_ref()
                    .map(|entry| entry.content.as_str()),
                expected
            );
        }
    }

    #[test]
    fn recovered_snapshot_rejects_external_edits_even_with_rolled_back_mtime() {
        let mut session = session();
        append_write(&mut session, "known");
        let map = new_read_file_state_map();
        restore_file_state(&session, Path::new("/workspace"), &map, &HashMap::new());
        for mtime in [-100, 0, 100] {
            assert!(tool_file::check_read_before_write(
                &map,
                Path::new(FILE),
                mtime,
                "known",
                false
            )
            .is_ok());
            assert!(tool_file::check_read_before_write(
                &map,
                Path::new(FILE),
                mtime,
                "external",
                false
            )
            .is_err());
        }
    }

    #[test]
    fn write_bom_matches_guard_decoding_without_double_stripping_read_content() {
        let mut written = session();
        append_write(&mut written, "\u{feff}known\r\n");
        assert_eq!(restored(&written).unwrap().content, "known\r\n");
        let mut read = session();
        // Read has already removed one disk BOM; a remaining U+FEFF is content.
        append_read(&mut read, "\u{feff}known");
        assert_eq!(restored(&read).unwrap().content, "\u{feff}known");
    }

    #[test]
    fn partial_reads_only_retain_consistent_prior_whole_snapshots() {
        for prior in [
            None,
            Some("one\ntwo\n"),
            Some("one\nchanged\n"),
            Some("one\ntwo\nthree\n"),
        ] {
            let mut session = session();
            if let Some(prior) = prior {
                append_read(&mut session, prior);
            }
            let id = call(
                &mut session,
                "Read",
                json!({"file_path":FILE,"offset":2,"limit":1}),
            );
            result(
                &mut session,
                id,
                Some(json!({"type":"text","file":{
                    "filePath":FILE,"content":"two\n","startLine":2,"numLines":1,"totalLines":3
                }})),
                false,
            );
            let expected = prior.filter(|prior| *prior == "one\ntwo\n");
            assert_eq!(
                restored(&session)
                    .as_ref()
                    .map(|entry| entry.content.as_str()),
                expected
            );
        }
    }

    #[test]
    fn capped_prefix_cannot_be_confused_with_trailing_newline_phantom_line() {
        for (content, limit, expected) in [
            ("one\n", 1, None),
            ("one\n", 2, Some("one\n")),
            ("one\ntwo", 2, Some("one\ntwo")),
        ] {
            let mut session = session();
            let id = call(
                &mut session,
                "Read",
                json!({"file_path":FILE,"limit":limit}),
            );
            result(&mut session, id, Some(read_data(content)), false);
            assert_eq!(
                restored(&session)
                    .as_ref()
                    .map(|entry| entry.content.as_str()),
                expected
            );
        }
        let mut session = session();
        append_read(&mut session, "one\ntwo");
        let id = call(&mut session, "Read", json!({"file_path":FILE,"limit":1}));
        result(&mut session, id, Some(read_data("one\n")), false);
        assert_eq!(restored(&session).unwrap().content, "one\ntwo");
    }

    #[test]
    fn truncated_nontext_or_malformed_read_invalidates_prior_snapshot() {
        let mut truncated = read_data("old");
        truncated["file"]["truncatedByTokenCap"] = json!(true);
        let mut wrong_count = read_data("old");
        wrong_count["file"]["numLines"] = json!(99);
        for data in [
            truncated,
            wrong_count,
            json!({"type":"image","file":{"filePath":FILE,"content":"old"}}),
            json!({"type":"notebook","file":{"filePath":FILE,"cells":[]}}),
            json!({"type":"text","file":{"filePath":FILE}}),
        ] {
            let mut session = session();
            append_read(&mut session, "old");
            let id = call(&mut session, "Read", json!({"file_path":FILE}));
            result(&mut session, id, Some(data), false);
            assert!(restored(&session).is_none());
        }
    }

    #[test]
    fn unchanged_stub_preserves_existing_snapshot_but_cannot_create_one() {
        for has_prior in [false, true] {
            let mut session = session();
            if has_prior {
                append_read(&mut session, "known");
            }
            let id = call(&mut session, "Read", json!({"file_path":FILE}));
            result(
                &mut session,
                id,
                Some(json!({"type":"file_unchanged","file":{"filePath":FILE}})),
                false,
            );
            assert_eq!(restored(&session).is_some(), has_prior);
        }
    }

    #[test]
    fn edits_apply_actual_structured_patch_instead_of_unadapted_input_strings() {
        let mut session = session();
        append_read(&mut session, "a ‘quoted’ string\nlast");
        let before = "a ‘quoted’ string\nlast";
        let after = "a changed string\nLAST\n";
        let id = call(
            &mut session,
            "Edit",
            json!({
                "file_path":FILE,"old_string":"'quoted'","new_string":"wrong if replayed"
            }),
        );
        result(
            &mut session,
            id,
            Some(json!({
                "filePath":FILE,"originalFile":before,
                "oldString":"'quoted'","newString":"wrong if replayed",
                "structuredPatch":tool_file::structured_patch::build_structured_patch(before, after)
            })),
            false,
        );
        let entry = restored(&session).unwrap();
        assert_eq!(entry.content, after);
        assert!(!entry.from_read);
    }

    #[test]
    fn edit_replay_recovers_crlf_only_when_prior_snapshot_proves_it() {
        let mut session = session();
        append_read(&mut session, "one\r\ntwo\r\n");
        let id = call(&mut session, "Edit", json!({"file_path":FILE}));
        result(
            &mut session,
            id,
            Some(json!({
                "filePath":FILE,"originalFile":"one\ntwo\n",
                "structuredPatch":tool_file::structured_patch::build_structured_patch("one\ntwo\n", "ONE\ntwo\n")
            })),
            false,
        );
        assert_eq!(restored(&session).unwrap().content, "ONE\r\ntwo\r\n");
    }

    #[test]
    fn edit_replay_uses_real_line_ending_codec_for_mixed_and_cr_only_files() {
        for prior in ["one\r\ntwo\n", "one\rtwo\r", "one\r\ntwo\r\nlast\n"] {
            let (before, encoding, ending) =
                tool_file::file_meta::read_with_metadata(prior.as_bytes());
            let after = before.replace("one", "ONE");
            let expected = String::from_utf8(tool_file::file_meta::encode_with_metadata(
                &after, encoding, ending,
            ))
            .unwrap();
            let mut session = session();
            append_read(&mut session, prior);
            let id = call(&mut session, "Edit", json!({"file_path":FILE}));
            result(
                &mut session,
                id,
                Some(json!({
                    "filePath":FILE,"originalFile":before,
                    "structuredPatch":tool_file::structured_patch::build_structured_patch(&before, &after)
                })),
                false,
            );
            assert_eq!(restored(&session).unwrap().content, expected);
        }
    }

    #[test]
    fn structured_patch_preserves_empty_crlf_newline_and_multiple_hunks() {
        let long_before: String = (0..40).map(|line| format!("line {line}\n")).collect();
        let long_after = long_before
            .replace("line 1\n", "changed\n")
            .replace("line 30\n", "other\n");
        for (before, after) in [
            ("", "new"),
            ("old", ""),
            ("old", "new"),
            ("old\n", "new"),
            ("old", "new\n"),
            ("same\n", "same\n"),
            ("one\r\ntwo", "one\r\nTWO\r\n"),
            (long_before.as_str(), long_after.as_str()),
        ] {
            let patch = serde_json::to_value(tool_file::structured_patch::build_structured_patch(
                before, after,
            ))
            .unwrap();
            assert_eq!(
                apply_structured_patch(before, &patch).as_deref(),
                Some(after),
                "{before:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn malformed_patch_is_rejected_and_cannot_leave_superseded_snapshot() {
        let valid =
            json!([{"oldStart":1,"newStart":1,"oldLines":1,"newLines":1,"lines":["-old","+new"]}]);
        let mut wrong_context = valid.clone();
        wrong_context[0]["lines"][0] = json!("-wrong");
        let mut wrong_count = valid.clone();
        wrong_count[0]["newLines"] = json!(2);
        let mut wrong_position = valid.clone();
        wrong_position[0]["newStart"] = json!(2);
        let mut bad_marker = valid.clone();
        bad_marker[0]["lines"] = json!(["\\ No newline at end of file", "-old", "+new"]);
        for patch in [
            wrong_context,
            wrong_count,
            wrong_position,
            bad_marker,
            json!("preview"),
        ] {
            let mut session = session();
            append_read(&mut session, "old\n");
            let id = call(&mut session, "Edit", json!({"file_path":FILE}));
            result(
                &mut session,
                id,
                Some(json!({"filePath":FILE,"originalFile":"old\n","structuredPatch":patch})),
                false,
            );
            assert!(restored(&session).is_none());
        }
    }

    #[test]
    fn excluded_results_failures_unpaired_results_and_relative_paths_do_not_seed() {
        let mut excluded = session();
        let result_id = append_write(&mut excluded, "known");
        excluded.model_context_excluded_messages.insert(result_id);
        assert!(restored(&excluded).is_none());

        let mut failed = session();
        let id = call(
            &mut failed,
            "Write",
            json!({"file_path":FILE,"content":"known"}),
        );
        result(&mut failed, id, Some(write_data("known")), true);
        assert!(restored(&failed).is_none());

        let mut unpaired = session();
        result(
            &mut unpaired,
            ToolUseId::new(),
            Some(write_data("known")),
            false,
        );
        assert!(restored(&unpaired).is_none());

        let mut relative = session();
        let id = call(
            &mut relative,
            "Write",
            json!({"file_path":"file.txt","content":"known"}),
        );
        result(&mut relative, id, Some(write_data("known")), false);
        assert!(restored(&relative).is_none());
    }

    #[test]
    fn successful_unsupported_file_edit_invalidates_old_snapshot() {
        let mut session = session();
        append_write(&mut session, "old");
        let id = call(&mut session, "NotebookEdit", json!({"notebook_path":FILE}));
        result(
            &mut session,
            id,
            Some(json!({"filePath":FILE,"cells":[]})),
            false,
        );
        assert!(restored(&session).is_none());
    }

    #[test]
    fn absolute_model_paths_use_the_same_lexical_normalizer_as_live_tools() {
        let mut session = session();
        let id = call(
            &mut session,
            "Write",
            json!({"file_path":"/workspace/sub/../file.txt","content":"known"}),
        );
        result(&mut session, id, Some(write_data("known")), false);
        assert_eq!(restored(&session).unwrap().content, "known");
    }

    fn append_attachment(
        session: &mut SessionState,
        attachments: &mut HashMap<MessageId, Utf16JsonProjection>,
        data: Value,
    ) -> MessageId {
        let id = MessageId::new();
        let data = projection(data);
        let message =
            crate::conversation::ConversationOrchestrator::file_attachment_projection(id, &data)
                .unwrap();
        session.history.push(message);
        attachments.insert(id, data);
        id
    }

    fn attachment_data(content: &str) -> Value {
        let lines = content.bytes().filter(|byte| *byte == b'\n').count() + 1;
        json!({"type":"file","filename":FILE,"content":{"type":"text","file":{
            "filePath":FILE,"content":content,"startLine":1,"numLines":lines,"totalLines":lines
        }}})
    }

    #[test]
    fn matched_full_file_attachments_restore_in_history_order_with_tools() {
        for content in ["", "known", "known\n"] {
            let mut session = session();
            let mut attachments = HashMap::new();
            append_write(&mut session, "old");
            append_attachment(&mut session, &mut attachments, attachment_data(content));
            let map = new_read_file_state_map();
            restore_file_state(&session, Path::new("/workspace"), &map, &attachments);
            let entry = get(&map, Path::new(FILE)).unwrap();
            assert_eq!(entry.content, content);
            assert_eq!(entry.mtime_ms, i64::MIN);
            assert!(entry.from_read);
            append_write(&mut session, "newer");
            let map = new_read_file_state_map();
            restore_file_state(&session, Path::new("/workspace"), &map, &attachments);
            assert_eq!(get(&map, Path::new(FILE)).unwrap().content, "newer");
        }
    }

    #[test]
    fn file_attachments_require_exact_active_projection_and_complete_content() {
        for case in ["excluded", "mismatch", "partial", "reference", "truncated"] {
            let mut session = session();
            let mut attachments = HashMap::new();
            let mut data = attachment_data("known\n");
            if case == "partial" {
                data["content"]["file"]["totalLines"] = json!(9);
            }
            if case == "truncated" {
                data["content"]["file"]["truncatedByTokenCap"] = json!(true);
            }
            if case == "reference" {
                data = json!({"type":"compact_file_reference","filename":FILE});
            }
            let id = append_attachment(&mut session, &mut attachments, data);
            if case == "excluded" {
                session.model_context_excluded_messages.insert(id);
            }
            if case == "mismatch" {
                session.history[0] = ConversationMessage::user(id, "unrelated".into());
            }
            let map = new_read_file_state_map();
            restore_file_state(&session, Path::new("/workspace"), &map, &attachments);
            assert!(get(&map, Path::new(FILE)).is_none(), "{case}");
        }
    }

    #[test]
    fn duplicate_ids_and_multi_result_sidecars_are_not_authoritative() {
        let mut duplicate = session();
        let id = call(
            &mut duplicate,
            "Write",
            json!({"file_path":FILE,"content":"known"}),
        );
        result(&mut duplicate, id.clone(), Some(write_data("known")), false);
        result(&mut duplicate, id, Some(write_data("different")), false);
        assert!(restored(&duplicate).is_none());

        let mut reused_call = session();
        append_write(&mut reused_call, "old");
        let id = call(
            &mut reused_call,
            "Write",
            json!({"file_path":FILE,"content":"known"}),
        );
        let duplicate_call = reused_call.history.last().unwrap().clone();
        reused_call.history.push(duplicate_call);
        result(&mut reused_call, id, Some(write_data("known")), false);
        assert!(restored(&reused_call).is_none());

        let mut batch = session();
        let first = call(
            &mut batch,
            "Write",
            json!({"file_path":FILE,"content":"known"}),
        );
        let second = call(
            &mut batch,
            "Read",
            json!({"file_path":"/workspace/other.txt"}),
        );
        result(&mut batch, first, Some(write_data("known")), false);
        if let ConversationMessage::User { content, .. } = batch.history.last_mut().unwrap() {
            content.push(ContentBlock::ToolResult {
                tool_use_id: second,
                content: projection(json!("other result")),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            });
        }
        assert!(restored(&batch).is_none());
    }

    #[test]
    fn interleaved_results_replay_in_completion_order_and_respect_lru_limits() {
        let mut session = session();
        let first = call(
            &mut session,
            "Write",
            json!({"file_path":FILE,"content":"first"}),
        );
        let other = call(
            &mut session,
            "Write",
            json!({"file_path":"/workspace/other.txt","content":"other"}),
        );
        result(
            &mut session,
            other,
            Some(json!({"type":"create","filePath":"/workspace/other.txt","content":"other"})),
            false,
        );
        result(&mut session, first, Some(write_data("first")), false);
        let last = call(
            &mut session,
            "Write",
            json!({"file_path":FILE,"content":"last"}),
        );
        result(&mut session, last, Some(write_data("last")), false);
        let map = tool_api::read_file_state::new_read_file_state_map_with_limits(1, 100);
        restore_file_state(&session, Path::new("/workspace"), &map, &HashMap::new());
        assert_eq!(get(&map, Path::new(FILE)).unwrap().content, "last");
        assert!(get(&map, Path::new("/workspace/other.txt")).is_none());
    }
}
