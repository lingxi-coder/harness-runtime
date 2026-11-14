//! Rebuild only file snapshots proven by successful, model-visible tool history.
//!
//! Transcript rows do not carry the original file mtime. A recovered snapshot
//! therefore uses the oldest timestamp, forcing the existing write guard to
//! compare its content with the current file even if the mtime went backwards.
//! This replay never reads the filesystem or treats present-day bytes as read.

use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use lingxi_core::SessionState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tool_api::read_file_state::{ReadFileEntry, ReadFileStateMap};

#[derive(Clone)]
struct FileCall {
    name: String,
    path: PathBuf,
    input: Utf16JsonProjection,
}

#[derive(Clone)]
struct RecoveredFile {
    entry: ReadFileEntry,
    /// This is authoritative text. `entry.content` is only its display form.
    units: Vec<u16>,
}

impl RecoveredFile {
    fn publish(self, cache: &mut tool_api::read_file_state::ReadFileStateLru, path: PathBuf) {
        cache.set_with_requested_aliases_and_units(
            path,
            self.entry,
            true,
            vec![],
            Some(self.units),
        );
    }
}

/// Explicit acquisition metadata only. Paths cross the OS boundary losslessly
/// on Windows; invalid UTF-16 paths are unavailable on Unicode-only hosts.
pub(super) fn start_only_paths(attachment: &Utf16JsonProjection) -> Vec<PathBuf> {
    let pointers = match attachment.string_at("/type") {
        Some("instructions")
            if lingxi_core::host::instruction_announcements::is_valid_context_attachment(
                attachment,
            ) =>
        {
            (0..attachment.array_len("/files").unwrap_or_default())
                .filter_map(|index| {
                    let prefix = format!("/files/{index}");
                    (attachment
                        .at_pointer(&format!("{prefix}/startOnly"))
                        .and_then(|value| value.as_bool())
                        == Some(true))
                    .then(|| format!("{prefix}/path"))
                })
                .collect::<Vec<_>>()
        }
        Some("nested_memory")
            if attachment
                .at_pointer("/content/startOnly")
                .and_then(|value| value.as_bool())
                == Some(true)
                && attachment.string_units("/content/path") == attachment.string_units("/path")
                && attachment.string_units("/content/content").is_some() =>
        {
            vec!["/path".to_owned()]
        }
        _ => Vec::new(),
    };
    pointers
        .into_iter()
        .filter_map(|pointer| {
            let units = attachment.string_units(&pointer)?;
            #[cfg(windows)]
            let path = {
                use std::os::windows::ffi::OsStringExt;
                PathBuf::from(std::ffi::OsString::from_wide(&units))
            };
            #[cfg(not(windows))]
            let path = PathBuf::from(String::from_utf16(&units).ok()?);
            path.is_absolute()
                .then(|| crate::turn_loop::normalize_lexically(&path))
        })
        .collect()
}

fn exact_path(source: &Utf16JsonProjection, pointer: &str, cwd: &Path) -> Option<PathBuf> {
    // Keep the existing path admission boundary strict. Text may contain lone
    // UTF-16 units; a path must never be repaired into another file's name.
    let raw_path = String::from_utf16(&source.string_units(pointer)?).ok()?;
    Path::new(&raw_path)
        .is_absolute()
        .then(|| tool_file::normalize_model_file_path(&raw_path, cwd))
}

fn unsigned(value: JsonValueView<'_>) -> Option<u64> {
    let value = value.as_number()?;
    // JavaScript file/line indices cannot supply exact evidence beyond the
    // safe-integer range. Reject fractional, negative and nonfinite counts.
    (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= 9_007_199_254_740_991.0)
        .then(|| value as u64)
}

fn is_ascii(value: JsonValueView<'_>, expected: &str) -> bool {
    value
        .code_units()
        .is_some_and(|units| units.iter().copied().eq(expected.encode_utf16()))
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
    // A transcript can span cwd/worktree changes but lacks a cwd per tool row.
    // Only absolute model routes are unambiguous during historical replay.
    let pointer = if input.contains_property("file_path") {
        "/file_path"
    } else {
        "/notebook_path"
    };
    let path = exact_path(input, pointer, cwd)?;
    Some((
        id.clone(),
        FileCall {
            name: name.clone(),
            path,
            input: input.clone(),
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
    // Clipping is session authority, including instructions which preceded a
    // compact boundary. Rebuild only explicit producer metadata; a reminder
    // containing the same prose cannot manufacture a clipping fence.
    for message in &session.history {
        if let Some(attachment) = attachments.get(&message.id()) {
            for path in start_only_paths(attachment) {
                cache.mark_start_only(path);
            }
        }
    }
    for message in &history {
        // A diff is not a full snapshot. Never revive an older Write/Read
        // baseline after the transcript proves it was superseded.
        if let Some(source) = attachments
            .get(&message.id())
            .filter(|source| source.string_at("/type") == Some("edited_text_file"))
        {
            if let Some(path) = exact_path(source, "/filename", cwd) {
                cache.remove(&path);
            }
        }
        if let Some((path, entry)) = attachment_snapshot(message, attachments, cwd) {
            if let Some(entry) = entry {
                entry.publish(&mut cache, path);
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
                    let prior = cache.peek(&call.path).and_then(|entry| {
                        Some(RecoveredFile {
                            entry,
                            units: cache.get_content_units(&call.path)?,
                        })
                    });
                    let snapshot = match raw {
                        Some(data) => recover_snapshot(&call, data, prior.as_ref(), cwd),
                        // Writer-less in-memory histories have no raw envelope.
                        // A successful Write still proves its exact input bytes.
                        None if call.name == "Write" => call
                            .input
                            .string_units("/content")
                            .map(|units| write_snapshot(&units)),
                        _ => None,
                    };
                    if let Some(entry) = snapshot {
                        entry.publish(&mut cache, call.path);
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

/// Complete text evidence usable by the post-compact instruction cursor.
/// Path-only references and diffs cannot grant instruction visibility.
pub(crate) fn complete_file_attachment_path(source: &Utf16JsonProjection) -> Option<PathBuf> {
    if source.string_at("/type") != Some("file") {
        return None;
    }
    let filename = String::from_utf16(&source.string_units("/filename")?).ok()?;
    if !Path::new(&filename).is_absolute()
        || source.string_units("/filename") != source.string_units("/content/file/filePath")
        || source
            .at_pointer("/content/file/truncatedByTokenCap")
            .is_some_and(|value| value.as_bool() != Some(false))
    {
        return None;
    }
    let units = source.string_units("/content/file/content")?;
    let lines = units.iter().filter(|unit| **unit == 10).count() as u64 + 1;
    if unsigned(source.at_pointer("/content/file/startLine")?)? != 1
        || unsigned(source.at_pointer("/content/file/numLines")?)? != lines
        || unsigned(source.at_pointer("/content/file/totalLines")?)? != lines
    {
        return None;
    }
    Some(PathBuf::from(filename))
}

fn attachment_snapshot(
    message: &ConversationMessage,
    attachments: &HashMap<MessageId, Utf16JsonProjection>,
    cwd: &Path,
) -> Option<(PathBuf, Option<RecoveredFile>)> {
    let source = attachments.get(&message.id())?;
    if source.string_at("/type")? != "file"
        || crate::conversation::ConversationOrchestrator::file_attachment_projection(
            message.id(),
            source,
        )
        .as_ref()
            != Some(message)
    {
        return None;
    }
    let path = exact_path(source, "/filename", cwd)?;
    let entry = (|| {
        if exact_path(source, "/content/file/filePath", cwd)? != path
            || source
                .at_pointer("/content/file/truncatedByTokenCap")
                .is_some_and(|value| value.as_bool() != Some(false))
        {
            return None;
        }
        let units = source.string_units("/content/file/content")?;
        // Attachments count the final empty fragment, even for empty files.
        let lines = units.iter().filter(|unit| **unit == 10).count() as u64 + 1;
        if unsigned(source.at_pointer("/content/file/startLine")?)? != 1
            || unsigned(source.at_pointer("/content/file/numLines")?)? != lines
            || unsigned(source.at_pointer("/content/file/totalLines")?)? != lines
        {
            return None;
        }
        Some(snapshot(units, true))
    })();
    Some((path, entry))
}

fn snapshot(units: Vec<u16>, from_read: bool) -> RecoveredFile {
    RecoveredFile {
        entry: ReadFileEntry {
            content: String::from_utf16_lossy(&units),
            mtime_ms: i64::MIN,
            offset: None,
            limit: None,
            from_read,
            seeded_from_context: false,
            is_partial_view: false,
        },
        units,
    }
}

fn without_bom(units: &[u16]) -> &[u16] {
    units.strip_prefix(&[0xfeff]).unwrap_or(units)
}

fn write_snapshot(units: &[u16]) -> RecoveredFile {
    // Write's output contains exact source text, including its optional BOM.
    // Match the live Write publication and subsequent stale-content comparison,
    // without repairing unpaired UTF-16 units or guessing the disk codec.
    snapshot(
        tool_file::file_meta::normalize_to_lf_units(without_bom(units)),
        false,
    )
}

fn recover_snapshot(
    call: &FileCall,
    data: &Utf16JsonProjection,
    prior: Option<&RecoveredFile>,
    cwd: &Path,
) -> Option<RecoveredFile> {
    let result_path = match call.name.as_str() {
        "Read" => "/file/filePath",
        "NotebookEdit" => "/notebook_path",
        _ => "/filePath",
    };
    if exact_path(data, result_path, cwd)? != call.path {
        return None;
    }
    match call.name.as_str() {
        "Read" if data.string_at("/type")? == "file_unchanged" => prior.cloned(),
        "Read" => {
            if data.string_at("/type")? != "text"
                || data
                    .at_pointer("/file/truncatedByTokenCap")
                    .is_some_and(|value| value.as_bool() != Some(false))
            {
                return None;
            }
            let units = data.string_units("/file/content")?;
            let start = unsigned(data.at_pointer("/file/startLine")?)?;
            let count = unsigned(data.at_pointer("/file/numLines")?)?;
            let total = unsigned(data.at_pointer("/file/totalLines")?)?;
            // Native joins selected line fragments with LF and includes the
            // terminal empty fragment. Earlier histories kept the delimiter
            // on each selected line instead; validate each shape separately.
            let native_count = units.split(|unit| *unit == 10).count() as u64;
            let native_view = count == native_count;
            let delimited_view = count == units.split_inclusive(|unit| *unit == 10).count() as u64;
            if start == 0 || count > total || (!native_view && !delimited_view) {
                return None;
            }
            // A capped prefix can mimic a trailing-newline whole file. Only
            // explicit EOF evidence authorizes a complete restored snapshot.
            let reached_eof = match call.input.at_pointer("/limit") {
                None => true,
                Some(limit) if limit.is_null() => true,
                Some(limit) => unsigned(limit)
                    .is_some_and(|limit| count < limit || (count == limit && count == total)),
            };
            if start == 1
                && reached_eof
                && ((native_view && total == native_count)
                    || (delimited_view && total == total_lines_units(&units)))
            {
                return Some(snapshot(units, true));
            }
            // A partial observation must agree with the exact prior whole
            // snapshot; equal display replacements do not establish equality.
            let prior = prior?;
            let first = usize::try_from(start.checked_sub(1)?).ok()?;
            let count = usize::try_from(count).ok()?;
            let end = first.checked_add(count)?;
            if native_view {
                let lines = prior.units.split(|unit| *unit == 10).collect::<Vec<_>>();
                if total == lines.len() as u64 {
                    if let Some(selected) = lines.get(first..end) {
                        let selected = selected
                            .iter()
                            .map(|line| line.strip_suffix(&[13]).unwrap_or(line))
                            .collect::<Vec<_>>();
                        if selected.join(&10) == units {
                            return Some(prior.clone());
                        }
                    }
                }
            }
            if delimited_view && total == total_lines_units(&prior.units) {
                let lines = prior
                    .units
                    .split_inclusive(|unit| *unit == 10)
                    .collect::<Vec<_>>();
                if lines
                    .get(first..end)
                    .is_some_and(|lines| lines.concat() == units)
                {
                    return Some(prior.clone());
                }
            }
            None
        }
        "Write" => {
            if !matches!(data.string_at("/type")?, "create" | "update") {
                return None;
            }
            Some(write_snapshot(&data.string_units("/content")?))
        }
        "Edit" => {
            let before = data.string_units("/originalFile")?;
            let mut after =
                apply_structured_patch_units(&before, data.at_pointer("/structuredPatch")?)?;
            // Preserve the established ending reconstruction only when the
            // prior exact snapshot proves it; never consult present-day disk.
            if let Some(prior) = prior {
                let normalized = tool_file::file_meta::normalize_to_lf_units(&prior.units);
                if normalized == before || normalized == without_bom(&before) {
                    after = apply_line_ending(
                        &after,
                        tool_file::file_meta::detect_line_ending_units(&prior.units),
                    );
                }
            }
            // Edit publishes the reconstructed disk line endings, unlike
            // Write's LF-normalized publication.
            let mut recovered = snapshot(without_bom(&after).to_vec(), false);
            // The full originalFile sidecar is UI/replay evidence, not proof
            // that a truncated Read's unseen suffix entered model context.
            // Live Edit preserves this fence until a complete Read replaces it.
            recovered.entry.is_partial_view = match data.at_pointer("/contentNotInModelContext") {
                Some(value) => value.as_bool()?,
                None => false,
            } || prior
                .is_some_and(|prior| prior.entry.is_partial_view);
            Some(recovered)
        }
        "NotebookEdit" => {
            // A native successful notebook result carries the entire updated
            // source. Validate its JSON structure without reserializing it or
            // trusting just one cell's possibly transformed source.
            data.string_units("/new_source")?;
            data.string_units("/language")?;
            if !matches!(data.string_at("/cell_type")?, "code" | "markdown")
                || !matches!(
                    data.string_at("/edit_mode")?,
                    "replace" | "insert" | "delete"
                )
                || data
                    .at_pointer("/error")
                    .is_some_and(|error| !is_ascii(error, ""))
            {
                return None;
            }
            let original = data.string_units("/original_file")?;
            let updated = data.string_units("/updated_file")?;
            for units in [&original, &updated] {
                let notebook = Utf16JsonProjection::parse_utf16_json(without_bom(units)).ok()?;
                notebook.array_len("/cells")?;
            }
            Some(snapshot(without_bom(&updated).to_vec(), false))
        }
        _ => None,
    }
}

fn total_lines_units(content: &[u16]) -> u64 {
    if content.is_empty() {
        0
    } else {
        content.iter().filter(|unit| **unit == 10).count() as u64 + 1
    }
}

fn apply_line_ending(content: &[u16], ending: tool_file::file_meta::LineEnding) -> Vec<u16> {
    use tool_file::file_meta::LineEnding;
    match ending {
        LineEnding::Lf => content.to_vec(),
        LineEnding::Crlf | LineEnding::Cr => {
            let mut result = Vec::new();
            for unit in tool_file::file_meta::normalize_to_lf_units(content) {
                if unit == 10 {
                    if ending == LineEnding::Crlf {
                        result.push(13);
                    }
                    result.push(if ending == LineEnding::Cr { 13 } else { 10 });
                } else {
                    result.push(unit);
                }
            }
            result
        }
    }
}

/// Apply complete jsdiff hunks by exact code units, without fuzzy oldString
/// matching or UTF-8 replacement decoding at any comparison boundary.
fn apply_structured_patch_units(before: &[u16], patch: JsonValueView<'_>) -> Option<Vec<u16>> {
    let original = before
        .split_inclusive(|unit| *unit == 10)
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut cursor = 0;
    for hunk_index in 0..patch.array_len()? {
        let hunk = patch.get_index(hunk_index)?;
        let old_start = usize::try_from(unsigned(hunk.get_property("oldStart")?)?)
            .ok()?
            .checked_sub(1)?;
        let new_start = usize::try_from(unsigned(hunk.get_property("newStart")?)?)
            .ok()?
            .checked_sub(1)?;
        let old_count = usize::try_from(unsigned(hunk.get_property("oldLines")?)?).ok()?;
        let new_count = usize::try_from(unsigned(hunk.get_property("newLines")?)?).ok()?;
        output.extend(
            original
                .get(cursor..old_start)?
                .iter()
                .map(|line| line.to_vec()),
        );
        if new_start != output.len() {
            return None;
        }
        cursor = old_start;
        let output_start = output.len();
        let lines = hunk.get_property("lines")?;
        let mut index = 0;
        while index < lines.array_len()? {
            let line = lines.get_index(index)?.code_units()?;
            let (&prefix, body) = line.split_first()?;
            if !matches!(prefix, 32 | 45 | 43) || body.contains(&10) {
                return None;
            }
            let mut text = body.to_vec();
            if lines
                .get_index(index + 1)
                .is_some_and(|line| is_ascii(line, "\\ No newline at end of file"))
            {
                index += 1;
            } else {
                text.push(10);
            }
            if prefix != 43 {
                if *original.get(cursor)? != text {
                    return None;
                }
                cursor += 1;
            }
            if prefix != 45 {
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
    output.extend(original.get(cursor..)?.iter().map(|line| line.to_vec()));
    if output
        .iter()
        .take(output.len().saturating_sub(1))
        .any(|line| !line.ends_with(&[10]))
    {
        return None;
    }
    Some(output.concat())
}

#[cfg(test)]
fn total_lines(content: &str) -> u64 {
    total_lines_units(&content.encode_utf16().collect::<Vec<_>>())
}

#[cfg(test)]
fn apply_structured_patch(before: &str, patch: &serde_json::Value) -> Option<String> {
    let patch = Utf16JsonProjection::from_serde_value(patch).ok()?;
    let result = apply_structured_patch_units(
        &before.encode_utf16().collect::<Vec<_>>(),
        patch.at_pointer("")?,
    )?;
    String::from_utf16(&result).ok()
}

#[cfg(test)]
#[path = "file_state_utf16_tests.rs"]
mod utf16_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::TaskDeliveryHistoryEnvelope;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{MessageId, SessionId};
    use serde_json::{json, Value};
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
                let expected = if read {
                    content.to_owned()
                } else {
                    String::from_utf16(&tool_file::file_meta::normalize_to_lf_units(
                        &content.encode_utf16().collect::<Vec<_>>(),
                    ))
                    .unwrap()
                };
                assert_eq!(entry.content, expected);
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
        assert_eq!(restored(&written).unwrap().content, "known\n");
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
            // jsdiff treats bare CR as content in one LF-delimited line.
            // The scalar test helper uses similar's broader CR tokenizer.
            let patch = if !before.contains('\n') && before.contains('\r') {
                json!([{
                    "oldStart":1,"oldLines":1,"newStart":1,"newLines":1,
                    "lines":[format!("-{before}"), "\\ No newline at end of file",
                        format!("+{after}"), "\\ No newline at end of file"]
                }])
            } else {
                json!(tool_file::structured_patch::build_structured_patch(
                    &before, &after
                ))
            };
            result(
                &mut session,
                id,
                Some(json!({
                    "filePath":FILE,"originalFile":before,
                    "structuredPatch":patch
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
