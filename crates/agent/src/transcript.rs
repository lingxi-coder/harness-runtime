//! Per-agent transcript writer.
//!
//! [`AgentTranscriptWriter`] appends one JSON line per
//! [`ConversationMessage`] to a transcript file under the agent's transcript
//! subdir.

use hooks::mods::{ModError, ModHost};
use lingxi_core::host::FileSystem;
use lingxi_core::types::{
    AgentId, ContentBlock, ConversationMessage, MessageId,
    utf16_json::{Utf16JsonKey, Utf16JsonProjection, Utf16JsonString},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Default)]
struct MessageRowIndexState {
    next_message_index: u64,
    by_message_id: HashMap<MessageId, u64>,
    writer: Option<AgentTranscriptWriter>,
}

tokio::task_local! {
    static MESSAGE_ROW_INDEXES: std::cell::RefCell<MessageRowIndexState>;
}

pub(crate) async fn scope_message_row_indexes<F, T>(
    resumed_history: Option<&[ConversationMessage]>,
    future: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let mut state = MessageRowIndexState::default();
    for message in resumed_history.unwrap_or_default() {
        if !message_has_session_row(message) {
            continue;
        }
        let index = state.next_message_index;
        state.next_message_index = state.next_message_index.saturating_add(1);
        state.by_message_id.insert(message.id(), index);
    }
    MESSAGE_ROW_INDEXES
        .scope(std::cell::RefCell::new(state), future)
        .await
}

fn message_has_session_row(message: &ConversationMessage) -> bool {
    match message {
        ConversationMessage::User { is_meta: true, .. }
        | ConversationMessage::User {
            is_compact_summary: true,
            ..
        }
        | ConversationMessage::User {
            is_visible_in_transcript_only: true,
            ..
        } => false,
        ConversationMessage::System {
            subtype: Some(subtype),
            ..
        } if subtype.starts_with("agent_") => false,
        _ => true,
    }
}

/// Assign or retrieve the stable session-agent row index for one visible
/// message. The same task-local allocator feeds the live observer and the
/// transcript entry, so later tombstone deletion leaves an index gap.
pub(crate) fn message_row_index(message: &ConversationMessage) -> Option<u64> {
    if !message_has_session_row(message) {
        return None;
    }
    MESSAGE_ROW_INDEXES
        .try_with(|indexes| {
            let mut indexes = indexes.borrow_mut();
            if let Some(index) = indexes.by_message_id.get(&message.id()) {
                return Some(*index);
            }
            let index = indexes.next_message_index;
            indexes.next_message_index = indexes.next_message_index.saturating_add(1);
            indexes.by_message_id.insert(message.id(), index);
            Some(index)
        })
        .ok()
        .flatten()
}

pub(crate) fn advance_message_row_index(next_message_index: u64) {
    let _ = MESSAGE_ROW_INDEXES.try_with(|indexes| {
        let mut indexes = indexes.borrow_mut();
        indexes.next_message_index = indexes.next_message_index.max(next_message_index);
    });
}

pub(crate) fn set_message_row_index_writer(writer: Option<&AgentTranscriptWriter>) {
    let _ = MESSAGE_ROW_INDEXES.try_with(|indexes| {
        indexes.borrow_mut().writer = writer.cloned();
    });
}

pub(crate) async fn persist_current_message_index_high_water()
-> Result<(), lingxi_core::host::FsError> {
    let (writer, next_message_index) = MESSAGE_ROW_INDEXES
        .try_with(|indexes| {
            let indexes = indexes.borrow();
            (indexes.writer.clone(), indexes.next_message_index)
        })
        .ok()
        .unwrap_or((None, 0));
    let Some(writer) = writer else {
        return Ok(());
    };
    let _journal = writer.journal_lock.lock().await;
    writer.recover_attachment_intent().await?;
    writer
        .persist_message_index_high_water_value(next_message_index)
        .await
}

fn current_next_message_index() -> Option<u64> {
    MESSAGE_ROW_INDEXES
        .try_with(|indexes| indexes.borrow().next_message_index)
        .ok()
}

/// One line in the agent transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Owning agent.
    pub agent_id: AgentId,
    /// Wall-clock timestamp the line was recorded.
    pub timestamp: SystemTime,
    /// The conversation message itself.
    pub message: ConversationMessage,
    /// Stable stream index assigned by the live Agent row allocator. Hidden
    /// lifecycle rows do not receive one; deleting a row never renumbers the
    /// survivors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_index: Option<u64>,
    /// Complete host-created server-fallback API-error envelope, independent
    /// of the query-message projection in `message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_fallback_api_error_json: Option<String>,
    /// Terminal agent status for lifecycle records. Ordinary conversation
    /// entries omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Terminal failure detail. Kept out of ordinary message entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional display name of the spawned agent. Older transcripts omit
    /// this field; hosts fall back to the parked task row or agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Resolved agent type (for example `general-purpose`). Persisting this
    /// beside messages lets a live tail expose metadata before a parked task
    /// row exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Selected provider-local model used to resume this agent. Transient
    /// refusal-serving routes do not replace the logical selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider profile paired with [`Self::model`], when pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    /// Caller correlation id (`SubagentContext::correlation_id`, e.g.
    /// Fusion's `{run_id}:p{index}`), when the spawn set one. Lets a
    /// transcript be matched back to the run/panel that produced it (G011).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Host-stamped original attachment associated with the derived meta
    /// message. The restore loader uses this to reconstruct Mod provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_attachment: Option<Value>,
    /// UUID of the original attachment side row. Kept outside the attachment
    /// payload because native transcript rows store UUID as row metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_attachment_uuid: Option<String>,
}

/// Transcript projection for a host-created API-error row. The visible
/// `message` remains the accepted query projection, while `uuid` retains the
/// outer API-row identity alongside the complete envelope stored in the
/// extension field.
#[derive(Serialize)]
struct TranscriptEntryWithRowUuid<'a> {
    #[serde(flatten)]
    entry: &'a TranscriptEntry,
    uuid: MessageId,
}

fn transcript_entry_projection(
    entry: &TranscriptEntry,
    source_attachment: Option<&SourceAttachment>,
) -> Result<Utf16JsonProjection, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    let mut projection = Utf16JsonProjection::plain(serde_json::to_value(entry)?);
    project_message_utf16(&mut projection, &entry.message, "/message");
    if let Some(attachment) = source_attachment {
        projection.value["source_attachment"] = attachment.projection.value.clone();
        append_projection_sidecars(
            &mut projection,
            "/source_attachment",
            &attachment.projection,
        );
    }
    projection.validate()?;
    Ok(projection)
}

fn project_message_utf16(
    projection: &mut Utf16JsonProjection,
    message: &ConversationMessage,
    message_pointer: &str,
) {
    let blocks = match message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => Some(content),
        ConversationMessage::System { .. } => None,
    };
    if let Some(blocks) = blocks {
        for (index, block) in blocks.iter().enumerate() {
            if let ContentBlock::ToolUse { input_projection: Some(input), .. } = block {
                projection.set_pointer(&format!("{message_pointer}/content/{index}/input"), input.clone()).expect("typed tool input owns its valid display tree");
            }
            if let ContentBlock::ToolResult { content_projection: Some(output), content_blocks, .. } = block {
                block.projected_tool_result().expect("typed result projection is valid");
                let field = if content_blocks.is_some() { "content_blocks" } else { "content" };
                projection.set_pointer(&format!("{message_pointer}/content/{index}/{field}"), output.clone()).expect("typed result source field exists");
            }
            let ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } = block
            else {
                continue;
            };
            let text_pointer = format!("{message_pointer}/content/{index}/text");
            let block_pointer = format!("{message_pointer}/content/{index}");
            let text = String::from_utf16_lossy(utf16_code_units);
            if let Some(projected_block) = projection
                .value
                .pointer_mut(&block_pointer)
                .and_then(Value::as_object_mut)
            {
                projected_block.insert("type".into(), Value::String("text".into()));
                projected_block.insert("text".into(), Value::String(text));
                projected_block.remove("utf16_code_units");
            }
            if String::from_utf16(utf16_code_units).is_err() {
                projection.strings.push(Utf16JsonString {
                    pointer: text_pointer,
                    code_units: utf16_code_units.clone(),
                });
            }
        }
    }
}

fn append_projection_sidecars(
    target: &mut Utf16JsonProjection,
    pointer_prefix: &str,
    source: &Utf16JsonProjection,
) {
    target
        .strings
        .extend(source.strings.iter().map(|sidecar| Utf16JsonString {
            pointer: format!("{pointer_prefix}{}", sidecar.pointer),
            code_units: sidecar.code_units.clone(),
        }));
    target
        .keys
        .extend(source.keys.iter().map(|sidecar| Utf16JsonKey {
            pointer: format!("{pointer_prefix}{}", sidecar.pointer),
            placeholder: sidecar.placeholder.clone(),
            code_units: sidecar.code_units.clone(),
        }));
}

fn transcript_attachment_projection(
    entry: &TranscriptEntry,
    mut attachment: Utf16JsonProjection,
) -> Result<Utf16JsonProjection, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    attachment.validate()?;
    let mut projection = transcript_entry_projection(entry, None)?;
    projection.value["type"] = Value::String("attachment".into());
    projection.value["uuid"] = serde_json::to_value(entry.message.id())?;
    projection.value["attachment"] = std::mem::take(&mut attachment.value);
    append_projection_sidecars(&mut projection, "/attachment", &attachment);
    projection.validate()?;
    Ok(projection)
}

#[derive(Clone)]
struct SourceAttachment {
    uuid: String,
    projection: Utf16JsonProjection,
}

#[derive(Clone)]
struct AppendedRow {
    message: ConversationMessage,
    source_attachment: Option<SourceAttachment>,
}

/// One `content_block_stop` assistant row. `accepted` is the single query,
/// event, and persistence row after the through-hook projection. The original
/// source ToolUses are retained separately because their identity/input drive W1 even
/// when accepted Text blocks precede it in `accepted.content`.
#[derive(Debug, Clone)]
pub(crate) struct StagedAssistantRow {
    pub api_block_index: u32,
    pub accepted: ConversationMessage,
    pub source_tool_uses: Vec<ContentBlock>,
}

impl StagedAssistantRow {
    pub(crate) fn from_source(api_block_index: u32, message: ConversationMessage) -> Self {
        let source_tool_uses = source_tool_uses(&message);
        Self {
            api_block_index,
            accepted: message,
            source_tool_uses,
        }
    }
}

fn source_tool_uses(message: &ConversationMessage) -> Vec<ContentBlock> {
    match message {
        ConversationMessage::Assistant { content, .. } => content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

fn append_content_block(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text, citations } => {
            let mut value = serde_json::json!({"type":"text","text":text});
            if let Some(citations) = citations {
                value["citations"] = citations.clone().unwrap_or(Value::Null);
            }
            value
        }
        ContentBlock::TextJsUtf16 {
            utf16_code_units,
            citations,
            ..
        } => {
            let text = String::from_utf16_lossy(utf16_code_units);
            let mut value = serde_json::json!({"type":"text","text":text});
            if let Some(citations) = citations {
                value["citations"] = citations.clone().unwrap_or(Value::Null);
            }
            value
        }
        ContentBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
         .. } => serde_json::json!({
            "type":"tool_use",
            "id":provider_id.as_deref().unwrap_or_else(|| id.as_str()),
            "name":name,
            "input":input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            provider_tool_use_id,
            content_blocks,
         .. } => {
            let mut value = serde_json::json!({
                "type":"tool_result",
                "tool_use_id":provider_tool_use_id.as_deref().unwrap_or_else(|| tool_use_id.as_str()),
                "content":content_blocks.as_ref().map_or_else(
                    || Value::String(content.clone()),
                    |blocks| Value::Array(blocks.clone()),
                ),
            });
            if let Some(is_error) = is_error {
                value["is_error"] = Value::Bool(*is_error);
            }
            value
        }
        ContentBlock::ProviderContent { value, .. } => value.clone(),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

fn append_message_projection(message: &ConversationMessage) -> Value {
    let (kind, name, role, is_meta, content) = match message {
        ConversationMessage::User {
            content, is_meta, ..
        } => (
            "user",
            None,
            Some("user"),
            *is_meta,
            content.iter().map(append_content_block).collect::<Vec<_>>(),
        ),
        ConversationMessage::Assistant { content, .. } => (
            "assistant",
            None,
            Some("assistant"),
            false,
            content.iter().map(append_content_block).collect::<Vec<_>>(),
        ),
        ConversationMessage::System {
            content, subtype, ..
        } => (
            "system",
            subtype.as_deref(),
            None,
            false,
            if content.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({"type":"text","text":content})]
            },
        ),
    };
    let mut result = serde_json::json!({"type":kind,"content":content});
    if let Some(name) = name {
        result["name"] = Value::String(name.to_owned());
    }
    if let Some(role) = role {
        result["role"] = Value::String(role.to_owned());
    }
    if is_meta {
        result["isMeta"] = Value::Bool(true);
    }
    result
}


fn append_text_parts(
    value: &Value,
    pointer: &str,
    strings: &[hooks::mods::ModUtf16StringSidecar],
) -> Option<(String, Option<Vec<u16>>)> {
    let text = value.as_str()?.to_owned();
    let code_units = strings
        .iter()
        .find(|sidecar| sidecar.pointer == pointer)
        .map(|sidecar| sidecar.code_units.clone());
    if code_units.as_ref().is_some_and(|units| {
        String::from_utf16(units).is_ok() || String::from_utf16_lossy(units) != text
    }) {
        return None;
    }
    Some((text, code_units))
}

fn append_block_matches(
    original: &ContentBlock,
    incoming: &Value,
    pointer: &str,
    strings: &[hooks::mods::ModUtf16StringSidecar],
) -> bool {
    if incoming.get("type").and_then(Value::as_str) != Some("text") {
        return append_content_block(original) == *incoming;
    }
    let Some((text, incoming_units)) = incoming
        .get("text")
        .and_then(|value| append_text_parts(value, pointer, strings))
    else {
        return false;
    };
    let (expected_text, exact_text_matches) = match original {
        ContentBlock::Text {
            text: source_text, ..
        } => (
            source_text.clone(),
            incoming_units.is_none() && text == *source_text,
        ),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => {
            let matches = match String::from_utf16(utf16_code_units) {
                Ok(exact) => incoming_units.is_none() && text == exact,
                Err(_) => incoming_units.as_deref() == Some(utf16_code_units.as_slice()),
            };
            (String::from_utf16_lossy(utf16_code_units), matches)
        }
        ContentBlock::ProviderContent {
            protocol,
            value: source,
        } if protocol.as_str() == "anthropic_messages"
            && source.get("type").and_then(Value::as_str) == Some("text") =>
        {
            let Some(source_text) = source.get("text").and_then(Value::as_str) else {
                return false;
            };
            (
                source_text.to_owned(),
                incoming_units.is_none() && text == source_text,
            )
        }
        _ => return false,
    };
    if !exact_text_matches {
        return false;
    }
    let mut normalized = incoming.clone();
    normalized["text"] = Value::String(expected_text);
    append_content_block(original) == normalized
}

fn append_new_text_block(
    incoming: &Value,
    pointer: &str,
    strings: &[hooks::mods::ModUtf16StringSidecar],
) -> Option<ContentBlock> {
    let (text, code_units) = incoming
        .get("text")
        .and_then(|value| append_text_parts(value, pointer, strings))?;
    Some(match code_units {
        Some(utf16_code_units) => ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations: Some(None),
        },
        None => ContentBlock::Text {
            text,
            citations: Some(None),
        },
    })
}

fn append_exact_message(message: &ConversationMessage) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, String> {
    message.project_native_content(append_message_projection(message), "/content").map_err(|error| error.to_string())
}

fn append_projection_matches(
    expected: &ConversationMessage,
    actual: &serde_json::Value,
    actual_strings: &[hooks::mods::ModUtf16StringSidecar],
    actual_keys: &[hooks::mods::ModUtf16KeySidecar],
) -> bool {
    let actual = hooks::mods::ModUtf16ValueProjection {
        value: serde_json::json!({"message":actual}), strings: actual_strings.to_vec(), keys: actual_keys.to_vec(),
    }.into_core_projection().and_then(|projection| projection.subprojection("/message").map_err(|error| hooks::mods::ModError::Protocol(error.to_string())))
        .and_then(|projection| projection.to_json_string().map_err(|error| hooks::mods::ModError::Protocol(error.to_string())));
    let expected = append_exact_message(expected).and_then(|projection| projection.to_json_string().map_err(|error| error.to_string()));
    matches!((actual, expected), (Ok(actual), Ok(expected)) if actual == expected)
}

fn append_identity_key(
    kind: &str,
    tool_identity: Option<&str>,
    counts: &mut HashMap<String, usize>,
) -> Option<String> {
    if matches!(kind, "text" | "image" | "document") {
        return None;
    }
    if kind == "tool_use" {
        if let Some(id) = tool_identity {
            return Some(format!("tool_use:{id}"));
        }
    } else if kind == "tool_result" {
        if let Some(id) = tool_identity {
            return Some(format!("tool_result:{id}"));
        }
    }
    let index = counts.entry(kind.to_owned()).or_default();
    let key = format!("{kind}#{index}");
    *index += 1;
    Some(key)
}

fn rewrite_tool_result_blocks(original: Option<&Vec<Value>>, incoming: &[Value],
    original_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    incoming_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> Vec<Value> {
    let original = original.map_or(&[][..], Vec::as_slice);
    let mut used = vec![false; original.len()];
    incoming
        .iter()
        .enumerate()
        .filter_map(|(incoming_index, block)| {
            if let Some((index, old)) = original
                .iter()
                .enumerate()
                .find(|(index, old)| {
                    if used[*index] { return false; }
                    if let (Some(original), Some(incoming)) = (original_projection, incoming_projection) {
                        let old = original.subprojection(&format!("/{index}")).and_then(|p| p.to_json_string());
                        let new = incoming.subprojection(&format!("/{incoming_index}")).and_then(|p| p.to_json_string());
                        return matches!((old, new), (Ok(old), Ok(new)) if old == new);
                    }
                    *old == block
                })
            {
                used[index] = true;
                return Some(if incoming_projection.is_some() { block.clone() } else { old.clone() });
            }
            if block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            {
                return Some(serde_json::json!({"type":"text","text":block["text"]}));
            }
            None
        })
        .collect()
}

fn rewrite_tool_result(original: &ContentBlock, incoming: &Value,
    source: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> ContentBlock {
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error: _,
        provider_tool_use_id,
        content_blocks,
        content_projection,
     .. } = original
    else {
        return original.clone();
    };
    let valid_content = incoming.get("content").is_none_or(|value| match value {
        Value::String(_) => true,
        Value::Array(blocks) => blocks
            .iter()
            .all(|block| block.get("type").and_then(Value::as_str).is_some()),
        _ => false,
    });
    let valid_is_error = incoming.get("is_error").is_none_or(Value::is_boolean);
    if !valid_content || !valid_is_error {
        return original.clone();
    }

    let mut next_content = content.clone();
    let mut next_content_blocks = content_blocks.clone();
    if let Some(value) = incoming.get("content") {
        match value {
            Value::String(text) => {
                next_content.clone_from(text);
                next_content_blocks = None;
            }
            Value::Array(blocks) => {
                if content_blocks.as_ref().is_some_and(|old| old == blocks) {
                    next_content_blocks = Some(blocks.clone());
                } else {
                    let blocks = rewrite_tool_result_blocks(content_blocks.as_ref(), blocks, content_projection.as_ref(), source);
                    next_content = blocks
                        .iter()
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    next_content_blocks = Some(blocks);
                }
            }
            _ => {}
        }
    }
    ContentBlock::ToolResult { content_projection: incoming.get("content").is_none().then(|| content_projection.clone()).flatten(),
        tool_use_id: tool_use_id.clone(),
        content: next_content,
        // Native makes an omitted flag logically false. Preserve its wire
        // absence instead of inheriting a source `true` from the old row.
        is_error: incoming.get("is_error").and_then(Value::as_bool),
        provider_tool_use_id: provider_tool_use_id.clone(),
        content_blocks: next_content_blocks,
    }
}

fn rewrite_append_blocks(
    original: &[ContentBlock],
    incoming: &[Value],
    strings: &[hooks::mods::ModUtf16StringSidecar],
    source: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> Vec<ContentBlock> {
    let mut source_counts = HashMap::new();
    let source_keys = original
        .iter()
        .map(|block| {
            let projection = append_content_block(block);
            let kind = projection.get("type").and_then(Value::as_str)?;
            let identity = match block {
                ContentBlock::ToolUse {
                    id, provider_id, ..
                } => provider_id.as_deref().or(Some(id.as_str())),
                ContentBlock::ToolResult {
                    tool_use_id,
                    provider_tool_use_id,
                    ..
                } => provider_tool_use_id
                    .as_deref()
                    .or(Some(tool_use_id.as_str())),
                _ => None,
            };
            append_identity_key(kind, identity, &mut source_counts)
        })
        .collect::<Vec<_>>();
    let source_keys_set = source_keys
        .iter()
        .filter_map(Clone::clone)
        .collect::<HashSet<_>>();
    let source_tool_results = original
        .iter()
        .zip(&source_keys)
        .filter_map(|(block, key)| {
            matches!(block, ContentBlock::ToolResult { .. })
                .then(|| key.clone())
                .flatten()
        })
        .collect::<Vec<_>>();
    let unbound_results = incoming
        .iter()
        .filter(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("tool_use_id").and_then(Value::as_str).is_none()
        })
        .count();
    let can_bind_unidentified_results = unbound_results == source_tool_results.len();
    let mut unbound_result_index = 0;
    let mut incoming_counts = HashMap::new();
    let mut raised_sources = vec![false; original.len()];
    let mut active_anchor = None::<String>;
    let mut overlays = HashMap::<Option<String>, Vec<ContentBlock>>::new();
    let mut tool_result_overlays = HashMap::<String, (usize, Value)>::new();

    for (incoming_index, block) in incoming.iter().enumerate() {
        let text_pointer = format!("/message/content/{incoming_index}/text");
        let Some(kind) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        if matches!(kind, "text" | "image" | "document") {
            if let Some((index, old)) = original.iter().enumerate().find(|(index, old)| {
                !raised_sources[*index]
                    && source_keys[*index].is_none()
                    && append_block_matches(old, block, &text_pointer, strings)
            }) {
                raised_sources[index] = true;
                overlays
                    .entry(active_anchor.clone())
                    .or_default()
                    .push(old.clone());
            } else if kind == "text" {
                if let Some(text) =
                    append_new_text_block(block, &text_pointer, strings).filter(|text| match text {
                        ContentBlock::Text { text, .. }
                        | ContentBlock::TextJsUtf16 { text, .. } => !text.is_empty(),
                        _ => false,
                    })
                {
                    overlays
                        .entry(active_anchor.clone())
                        .or_default()
                        .push(text);
                }
            }
            continue;
        }

        let identity = match kind {
            "tool_use" => block.get("id").and_then(Value::as_str),
            "tool_result" => block.get("tool_use_id").and_then(Value::as_str),
            _ => None,
        };
        let generated_key = append_identity_key(kind, identity, &mut incoming_counts);
        let identity_key = if kind == "tool_result" && identity.is_none() {
            let key = can_bind_unidentified_results
                .then(|| source_tool_results.get(unbound_result_index).cloned())
                .flatten();
            unbound_result_index += 1;
            key
        } else {
            generated_key
        };
        let Some(identity_key) = identity_key.filter(|key| source_keys_set.contains(key)) else {
            continue;
        };
        active_anchor = Some(identity_key.clone());
        if kind == "tool_result" {
            tool_result_overlays.insert(identity_key, (incoming_index, block.clone()));
        }
    }

    let sentinel_index = original
        .iter()
        .position(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ToolResult { .. }
            )
        })
        .unwrap_or(original.len());
    let sentinel = overlays.remove(&None).unwrap_or_default();
    let mut rewritten = Vec::new();
    for index in 0..=original.len() {
        if index == sentinel_index {
            rewritten.extend(sentinel.iter().cloned());
        }
        if index == original.len() {
            break;
        }
        if let Some(identity_key) = &source_keys[index] {
            let block = match (&original[index], tool_result_overlays.get(identity_key)) {
                (ContentBlock::ToolResult { .. }, Some((incoming_index, incoming))) => {
                    let content_source = source.and_then(|source| source.subprojection(&format!("/message/content/{incoming_index}/content")).ok());
                    let mut result = rewrite_tool_result(&original[index], incoming, content_source.as_ref());
                    if incoming.get("is_error").is_none_or(Value::is_boolean) {
                        if let Some(exact) = content_source.filter(|exact| exact.value.is_string() || exact.value.is_array())
                        {
                            result.rebase_tool_result_projection(exact).expect("validated result content rewrite");
                        }
                    }
                    result
                }
                _ => original[index].clone(),
            };
            rewritten.push(block);
            if let Some(overlay) = overlays.get(&Some(identity_key.clone())) {
                rewritten.extend(overlay.iter().cloned());
            }
        }
    }
    rewritten
}

fn rewrite_append_message(
    original: &ConversationMessage,
    incoming: &Value,
    strings: &[hooks::mods::ModUtf16StringSidecar],
    source: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> Result<ConversationMessage, String> {
    let Some(content) = incoming.get("content").and_then(Value::as_array) else {
        return Err("session.append message.content must be an array".into());
    };
    Ok(match original {
        ConversationMessage::User {
            id,
            content: old_content,
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
         .. } => ConversationMessage::User { api_message_override: None,
            id: *id,
            content: rewrite_append_blocks(old_content, content, strings, source),
            is_meta: *is_meta,
            is_compact_summary: *is_compact_summary,
            is_visible_in_transcript_only: *is_visible_in_transcript_only,
        },
        ConversationMessage::Assistant {
            id,
            content: old_content,
            stop_reason,
         .. } => ConversationMessage::Assistant { per_turn_effort: None,
            id: *id,
            content: rewrite_append_blocks(old_content, content, strings, source),
            stop_reason: stop_reason.clone(),
        },
        ConversationMessage::System {
            id,
            subtype,
            compact_metadata,
            model_fallback,
            refusal_fallback,
            ..
        } => ConversationMessage::System { api_system: None,
            id: *id,
            content: content
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            subtype: subtype.clone(),
            compact_metadata: compact_metadata.clone(),
            model_fallback: model_fallback.clone(),
            refusal_fallback: refusal_fallback.clone(),
        },
    })
}

fn append_door_origin(
    message: &ConversationMessage,
    prior_history: &[ConversationMessage],
    model: Option<&str>,
) -> (String, Value) {
    match message {
        ConversationMessage::Assistant { .. } => (
            "response".into(),
            serde_json::json!({"kind":"model","model":model.unwrap_or("unknown")}),
        ),
        ConversationMessage::System { subtype, .. } => {
            let door = subtype.as_deref().map_or("notice", |subtype| {
                if subtype.ends_with("_boundary") {
                    "compaction"
                } else if subtype == "local_command" {
                    "command"
                } else {
                    "notice"
                }
            });
            (door.into(), serde_json::json!({"kind":"engine"}))
        }
        ConversationMessage::User {
            content,
            is_meta,
            is_compact_summary,
            ..
        } => {
            if *is_compact_summary {
                return ("compaction".into(), serde_json::json!({"kind":"engine"}));
            }
            if let Some(tool_use_id) = content.iter().find_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            }) {
                let tool = prior_history
                    .iter()
                    .rev()
                    .find_map(|message| {
                        let ConversationMessage::Assistant { content, .. } = message else {
                            return None;
                        };
                        content.iter().find_map(|block| match block {
                            ContentBlock::ToolUse { id, name, .. } if id == tool_use_id => {
                                Some(name.clone())
                            }
                            _ => None,
                        })
                    })
                    .unwrap_or_else(|| "unknown".into());
                return (
                    "tool-result".into(),
                    serde_json::json!({"kind":"tool","tool":tool}),
                );
            }
            if *is_meta {
                return ("note".into(), serde_json::json!({"kind":"engine"}));
            }
            ("prompt".into(), serde_json::json!({"kind":"coordinator"}))
        }
    }
}

fn source_attachment_projection(
    attachment: &SourceAttachment,
) -> Option<(Utf16JsonProjection, String, Value)> {
    let value = &attachment.projection.value;
    let kind = value.get("type")?.as_str()?;
    let content = value.get("content")?.as_array()?;
    let content = content
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    let door = if matches!(kind, "queued_command" | "poll_events" | "teammate_mailbox") {
        "delivery"
    } else if kind.starts_with("hook_") || kind == "async_hook_response" {
        "hook-context"
    } else {
        "attachment"
    };
    let origin = if kind == "hook_additional_context" {
        match value.get("hookName").and_then(Value::as_str) {
            Some(event @ ("prompt.submit" | "tool.call")) => {
                serde_json::json!({"kind":"plugin","event":event})
            }
            _ => serde_json::json!({
                "kind":"hook",
                "event":value.get("hookEvent").cloned().unwrap_or(Value::Null),
            }),
        }
    } else if let Some(event) = value.get("hookEvent") {
        serde_json::json!({"kind":"hook","event":event})
    } else {
        serde_json::json!({"kind":"engine"})
    };
    let mut message = Utf16JsonProjection::plain(serde_json::json!({
        "type":"attachment",
        "name":kind,
        "content":content.iter().map(|text| serde_json::json!({"type":"text","text":text})).collect::<Vec<_>>(),
    }));
    for (index, _) in content.iter().enumerate() {
        let code_units = attachment
            .projection
            .string_units(&format!("/content/{index}"))?;
        if String::from_utf16(&code_units).is_err() {
            message.strings.push(Utf16JsonString {
                pointer: format!("/content/{index}/text"),
                code_units,
            });
        }
    }
    Some((message, door.into(), origin))
}

fn source_attachment_result_strings(
    message: &Utf16JsonProjection,
) -> Vec<hooks::mods::ModUtf16StringSidecar> {
    let mut strings = message
        .strings
        .iter()
        .map(|sidecar| hooks::mods::ModUtf16StringSidecar {
            pointer: format!("/message{}", sidecar.pointer),
            code_units: sidecar.code_units.clone(),
        })
        .collect::<Vec<_>>();
    strings.sort_by(|left, right| left.pointer.cmp(&right.pointer));
    strings
}

async fn dispatch_append_event<F, Fut>(
    host: &Arc<ModHost>,
    cwd: &std::path::Path,
    input: hooks::mods::ModUtf16ValueProjection,
    core: F,
) -> Result<hooks::mods::ModDispatchOutcome, ModError>
where
    F: FnMut(hooks::mods::ModUtf16ValueProjection) -> Fut,
    Fut: std::future::Future<Output = Result<hooks::mods::ModUtf16ValueProjection, ModError>>,
{
    if let Some(session) = host.bound_session() {
        let log_session = session.clone();
        let toast_session = session.clone();
        let status_session = session.clone();
        host.dispatch_with_utf16_at_context(
            "session.append",
            input,
            cwd,
            Some(session.as_ref()),
            Some(cwd),
            None,
            None,
            lingxi_core::host::task_registry::FieldPresence::Missing,
            core,
            move |plugin, text| {
                let session = log_session.clone();
                async move { session.emit_mod_log(&plugin, &text).await }
            },
            move |plugin, text, timeout_ms| {
                let session = toast_session.clone();
                async move { session.emit_mod_toast(&plugin, &text, timeout_ms).await }
            },
            move |plugin, text| {
                let session = status_session.clone();
                async move { session.emit_mod_status(&plugin, text.as_deref()).await }
            },
        )
        .await
    } else {
        host.dispatch_with_utf16_at_context(
            "session.append",
            input,
            cwd,
            None,
            None,
            None,
            None,
            lingxi_core::host::task_registry::FieldPresence::Missing,
            core,
            |_, _| async {},
            |_, _, _| async {},
            |_, _| async {},
        )
        .await
    }
}

async fn mod_append_message_row(
    host: &Arc<ModHost>,
    cwd: &std::path::Path,
    agent_id: AgentId,
    model: Option<&str>,
    prior_history: &[ConversationMessage],
    original: &ConversationMessage,
) -> ConversationMessage {
    let uuid = original.id().as_uuid().to_string();
    let (door, origin) = append_door_origin(original, prior_history, model);
    let input = serde_json::json!({
        "message":append_message_projection(original),
        "door":door,
        "origin":origin,
        "uuid":uuid,
        "agentId":agent_id.as_uuid().to_string(),
    });
    let input_projection = match original.project_native_content(input.clone(), "/message/content")
        .map_err(|error| ModError::Protocol(error.to_string()))
        .and_then(hooks::mods::ModUtf16ValueProjection::from_core_projection)
    {
        Ok(projection) => projection,
        Err(error) => { tracing::warn!(%uuid, %error, "invalid child session.append source projection"); return original.clone(); }
    };
    let core_input = input.clone();
    let core_uuid = uuid.clone();
    let core_original = original.clone();
    let applied = Arc::new(Mutex::new(None::<(ConversationMessage, Value)>));
    let applied_by_core = applied.clone();
    let result = dispatch_append_event(
        host,
        cwd,
        input_projection,
        move |forwarded_projection| {
            let core_input = core_input.clone();
            let core_uuid = core_uuid.clone();
            let core_original = core_original.clone();
            let applied = applied_by_core.clone();
            async move {
                let input_utf16_strings = forwarded_projection.strings.clone();
                let source = forwarded_projection.into_core_projection()?;
                let forwarded = &source.value;
                for key in ["door", "origin", "uuid", "agentId"] {
                    if forwarded
                        .get(key)
                        .is_some_and(|value| core_input.get(key) != Some(value))
                    {
                        return Err(ModError::Hook(format!("session.append {key} is pinned")));
                    }
                }
                let Some(incoming) = forwarded.get("message") else {
                    return Err(ModError::Hook(
                        "session.append needs message.content".into(),
                    ));
                };
                let original_projection = append_message_projection(&core_original);
                for key in ["type", "name", "role", "isMeta"] {
                    if incoming
                        .get(key)
                        .is_some_and(|value| original_projection.get(key) != Some(value))
                    {
                        return Err(ModError::Hook(format!(
                            "session.append message.{key} is pinned"
                        )));
                    }
                }
                let rewritten =
                    rewrite_append_message(&core_original, incoming, &input_utf16_strings, Some(&source))
                        .map_err(ModError::Hook)?;
                let projected = append_exact_message(&rewritten).map_err(ModError::Hook)?;
                *applied
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((rewritten, projected.value.clone()));
                let mut result = Utf16JsonProjection::plain(serde_json::json!({"message":projected.value,"uuid":core_uuid}));
                result.set_pointer("/message", projected).map_err(|error| ModError::Protocol(error.to_string()))?;
                hooks::mods::ModUtf16ValueProjection::from_core_projection(result)
            }
        },
    )
    .await;
    match result {
        Ok(outcome) => {
            let result = outcome.result;
            let result_utf16_strings = outcome.result_utf16_strings;
            let result_utf16_keys = outcome.result_utf16_keys;
            let accepted = applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            match accepted {
                Some((rewritten, _projected))
                    if result["uuid"] == uuid
                        && append_projection_matches(
                            &rewritten,
                            &result["message"],
                            &result_utf16_strings,
                            &result_utf16_keys,
                        ) =>
                {
                    rewritten
                }
                _ => original.clone(),
            }
        }
        Err(error) => {
            tracing::warn!(%uuid, %error, "child session.append Mod dispatch failed; preserving original row");
            original.clone()
        }
    }
}

async fn mod_append_source_attachment(
    host: &Arc<ModHost>,
    cwd: &std::path::Path,
    agent_id: AgentId,
    original: &SourceAttachment,
) -> SourceAttachment {
    let Some((message, door, origin)) = source_attachment_projection(original) else {
        return original.clone();
    };
    let uuid = original.uuid.clone();
    let mut input = Utf16JsonProjection::plain(serde_json::json!({
        "message":message.value,
        "door":door,
        "origin":origin,
        "uuid":uuid,
        "agentId":agent_id.as_uuid().to_string(),
    }));
    input
        .strings
        .extend(message.strings.into_iter().map(|sidecar| Utf16JsonString {
            pointer: format!("/message{}", sidecar.pointer),
            code_units: sidecar.code_units,
        }));
    let core_input = input.value.clone();
    let core_original = original.clone();
    let core_message = input.value["message"].clone();
    let input_projection = hooks::mods::ModUtf16ValueProjection {
        value: input.value,
        strings: input
            .strings
            .into_iter()
            .map(|sidecar| hooks::mods::ModUtf16StringSidecar {
                pointer: sidecar.pointer,
                code_units: sidecar.code_units,
            })
            .collect(),
        keys: Vec::new(),
    };
    let applied = Arc::new(Mutex::new(None::<(SourceAttachment, Utf16JsonProjection)>));
    let applied_by_core = applied.clone();
    let result = dispatch_append_event(host, cwd, input_projection, move |forwarded_projection| {
        let core_input = core_input.clone();
        let core_original = core_original.clone();
        let core_message = core_message.clone();
        let applied = applied_by_core.clone();
        let forwarded = forwarded_projection.value;
        let input_utf16_strings = forwarded_projection.strings;
        async move {
            for key in ["door", "origin", "uuid", "agentId"] {
                if forwarded
                    .get(key)
                    .is_some_and(|value| core_input.get(key) != Some(value))
                {
                    return Err(ModError::Hook(format!("session.append {key} is pinned")));
                }
            }
            let Some(incoming) = forwarded.get("message") else {
                return Err(ModError::Hook(
                    "session.append needs message.content".into(),
                ));
            };
            for key in ["type", "name"] {
                if incoming
                    .get(key)
                    .is_some_and(|value| core_message.get(key) != Some(value))
                {
                    return Err(ModError::Hook(format!(
                        "session.append message.{key} is pinned"
                    )));
                }
            }
            let Some(blocks) = incoming.get("content").and_then(Value::as_array) else {
                return Err(ModError::Hook(
                    "session.append message.content must be an array".into(),
                ));
            };
            let Some(texts) = blocks
                .iter()
                .enumerate()
                .map(|(index, block)| {
                    if block.get("type").and_then(Value::as_str) != Some("text") {
                        return None;
                    }
                    let pointer = format!("/message/content/{index}/text");
                    let (text, exact_units) =
                        append_text_parts(block.get("text")?, &pointer, &input_utf16_strings)?;
                    Some((text, exact_units))
                })
                .collect::<Option<Vec<_>>>()
            else {
                return Err(ModError::Hook(
                    "session.append attachment content must remain text".into(),
                ));
            };
            let mut rewritten = core_original.clone();
            rewritten.projection.value["content"] =
                serde_json::json!(texts.iter().map(|(text, _)| text).collect::<Vec<_>>());
            rewritten
                .projection
                .strings
                .retain(|sidecar| !sidecar.pointer.starts_with("/content/"));
            rewritten.projection.keys.retain(|sidecar| {
                sidecar.pointer != "/content" && !sidecar.pointer.starts_with("/content/")
            });
            for (index, (_, exact_units)) in texts.iter().enumerate() {
                if let Some(exact_units) = exact_units {
                    rewritten.projection.strings.push(Utf16JsonString {
                        pointer: format!("/content/{index}"),
                        code_units: exact_units.clone(),
                    });
                }
            }
            rewritten.projection.validate().map_err(|error| {
                ModError::Hook(format!(
                    "session.append attachment projection is invalid: {error}"
                ))
            })?;
            let Some((projected, _, _)) = source_attachment_projection(&rewritten) else {
                return Err(ModError::Hook(
                    "session.append attachment projection is invalid".into(),
                ));
            };
            *applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((rewritten, projected.clone()));
            let result_strings = projected
                .strings
                .into_iter()
                .map(|sidecar| hooks::mods::ModUtf16StringSidecar {
                    pointer: format!("/message{}", sidecar.pointer),
                    code_units: sidecar.code_units,
                })
                .collect::<Vec<_>>();
            Ok(hooks::mods::ModUtf16ValueProjection {
                value: serde_json::json!({"message":projected.value,"uuid":core_original.uuid}),
                strings: result_strings,
                keys: Vec::new(),
            })
        }
    })
    .await;
    match result {
        Ok(outcome) => {
            let result = outcome.result;
            let mut actual_strings = outcome.result_utf16_strings;
            let mut actual_keys = outcome.result_utf16_keys;
            actual_strings.sort_by(|left, right| left.pointer.cmp(&right.pointer));
            actual_keys.sort_by(|left, right| left.pointer.cmp(&right.pointer));
            let accepted = applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            match accepted {
                Some((rewritten, projected))
                    if result["uuid"] == original.uuid
                        && result["message"] == projected.value
                        && source_attachment_result_strings(&projected) == actual_strings
                        && actual_keys.is_empty() =>
                {
                    rewritten
                }
                _ => {
                    tracing::warn!(
                        %uuid,
                        "child attachment session.append Mod did not accept its projection; preserving the original sidecar"
                    );
                    original.clone()
                }
            }
        }
        Err(error) => {
            tracing::warn!(%uuid, %error, "child attachment session.append Mod dispatch failed; preserving original row");
            original.clone()
        }
    }
}

#[derive(Clone, Default)]
struct TranscriptModelSelection {
    model: Option<String>,
    model_profile: Option<String>,
    mod_model: Option<String>,
}

/// Appends [`TranscriptEntry`] lines to a per-agent transcript file.
#[derive(Clone)]
pub struct AgentTranscriptWriter {
    /// Absolute path the transcript is written to.
    pub transcript_path: PathBuf,
    /// Owning agent id; stamped on every entry.
    pub agent_id: AgentId,
    /// Sandboxed filesystem used to read/write the transcript.
    fs: Option<Arc<dyn FileSystem>>,
    agent_name: Option<String>,
    agent_type: Option<String>,
    model_selection: Arc<Mutex<TranscriptModelSelection>>,
    correlation_id: Option<String>,
    journal_lock: Arc<tokio::sync::Mutex<()>>,
    source_attachments: Arc<Mutex<HashMap<MessageId, SourceAttachment>>>,
    mod_executor: Option<Arc<hooks::HookExecutorImpl>>,
    mod_cwd: Option<PathBuf>,
    appended_rows: Arc<Mutex<HashMap<MessageId, AppendedRow>>>,
    dispatched_attachment_rows: Arc<Mutex<HashSet<String>>>,
}

impl AgentTranscriptWriter {
    /// Construct a new writer that targets `transcript_path` for `agent_id`.
    #[must_use]
    pub fn new(transcript_path: PathBuf, agent_id: AgentId, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            transcript_path,
            agent_id,
            fs: Some(fs),
            agent_name: None,
            agent_type: None,
            model_selection: Arc::new(Mutex::new(TranscriptModelSelection::default())),
            correlation_id: None,
            journal_lock: Arc::new(tokio::sync::Mutex::new(())),
            source_attachments: Arc::new(Mutex::new(HashMap::new())),
            mod_executor: None,
            mod_cwd: None,
            appended_rows: Arc::new(Mutex::new(HashMap::new())),
            dispatched_attachment_rows: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Construct an in-memory append processor for child runs that have a Mod
    /// host but no transcript filesystem. Native `session.append` runs at the
    /// retained-row keep point independently of durable transcript storage.
    #[must_use]
    pub fn without_persistence(transcript_path: PathBuf, agent_id: AgentId) -> Self {
        Self {
            transcript_path,
            agent_id,
            fs: None,
            agent_name: None,
            agent_type: None,
            model_selection: Arc::new(Mutex::new(TranscriptModelSelection::default())),
            correlation_id: None,
            journal_lock: Arc::new(tokio::sync::Mutex::new(())),
            source_attachments: Arc::new(Mutex::new(HashMap::new())),
            mod_executor: None,
            mod_cwd: None,
            appended_rows: Arc::new(Mutex::new(HashMap::new())),
            dispatched_attachment_rows: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Attach display metadata that is copied onto each entry. This remains a
    /// builder so existing minimal/test callers can keep the old constructor.
    #[must_use]
    pub fn with_metadata(
        mut self,
        agent_name: Option<String>,
        agent_type: Option<String>,
        model: Option<String>,
        model_profile: Option<String>,
    ) -> Self {
        self.agent_name = agent_name;
        self.agent_type = agent_type;
        let mut selection = self
            .model_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        selection.model = model;
        selection.model_profile = model_profile;
        drop(selection);
        self
    }

    /// Attach the caller correlation id (G011) copied onto each entry.
    /// Separate from [`Self::with_metadata`] so existing callers of that
    /// builder are unaffected.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: Option<String>) -> Self {
        self.correlation_id = correlation_id;
        self
    }

    /// Configure child `session.append` dispatch. The executor remains the
    /// live source of the Mod host so registry changes between rows take effect.
    #[must_use]
    pub fn with_mod_append(
        mut self,
        executor: Option<Arc<hooks::HookExecutorImpl>>,
        cwd: PathBuf,
        model: Option<String>,
    ) -> Self {
        self.mod_executor = executor;
        self.mod_cwd = Some(cwd);
        self.model_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mod_model = model;
        self
    }

    fn model_selection(&self) -> TranscriptModelSelection {
        self.model_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Persist a committed logical selection even if no further model message
    /// is produced before the agent parks. Cloned writers share the same pair.
    pub(crate) async fn record_model_selection(
        &self,
        model: &str,
        model_profile: Option<&str>,
    ) -> Result<(), lingxi_core::host::FsError> {
        let _journal = self.journal_lock.lock().await;
        let projection = Utf16JsonProjection::plain(serde_json::json!({
            "type": "model-selection",
            "agent_id": self.agent_id,
            "timestamp": SystemTime::now(),
            "model": model,
            "model_profile": model_profile,
            "message": ConversationMessage::System {
                id: MessageId::new(),
                content: "Model selection changed".into(),
                subtype: Some("agent_model_selection".into()),
                api_system: None,
                compact_metadata: None,
                model_fallback: None,
                refusal_fallback: None,
            },
        }));
        self.append_projection_locked(projection).await?;
        let mut selection = self
            .model_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        selection.model = Some(model.to_string());
        selection.model_profile = model_profile.map(str::to_string);
        selection.mod_model = Some(model.to_string());
        Ok(())
    }

    /// Associate an original attachment with its derived model message before
    /// the transcript watermark writes that message.
    pub fn register_source_attachment(
        &self,
        message: &ConversationMessage,
        attachment: Utf16JsonProjection,
    ) {
        self.source_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                message.id(),
                SourceAttachment {
                    uuid: MessageId::new().as_uuid().to_string(),
                    projection: attachment,
                },
            );
    }

    /// Append one [`TranscriptEntry`] for `message`.
    pub async fn record(
        &self,
        message: &ConversationMessage,
    ) -> Result<(), lingxi_core::host::FsError> {
        let mut retained = message.clone();
        self.record_retained(&mut retained, &[]).await
    }

    /// Persist the visible query projection together with the full host API
    /// error envelope. The observer path carries the same typed row for
    /// memory-only sessions where this writer has no filesystem.
    pub async fn record_server_fallback_api_error_row(
        &self,
        row: &lingxi_core::host::ServerFallbackApiErrorRow,
    ) -> Result<(), lingxi_core::host::FsError> {
        let message = row.query_message();
        let mut entry = self.message_entry(&message);
        entry.server_fallback_api_error_json =
            Some(serde_json::to_string(row).expect("fallback API-error row serializes"));
        self.append_entry(&TranscriptEntryWithRowUuid {
            entry: &entry,
            uuid: row.uuid,
        })
        .await
    }

    /// Run the normal retained-row `session.append` hook before a synthetic
    /// API-error row is added to Agent history or persisted. The host envelope
    /// stays intact while its visible assistant content follows the accepted
    /// query projection.
    pub(crate) async fn accept_server_fallback_api_error_row(
        &self,
        row: &mut lingxi_core::host::ServerFallbackApiErrorRow,
        prior_history: &[ConversationMessage],
    ) -> Result<(), lingxi_core::host::FsError> {
        let mut message = row.query_message();
        self.accept_retained(&mut message, prior_history).await?;
        if let ConversationMessage::Assistant { content, .. } = message {
            row.message.content = content;
        }
        Ok(())
    }

    /// Remove persisted rows for a native server-fallback tombstone.
    ///
    /// Native `displayOnly` affects presentation only; transcript consumers
    /// still delete the row by UUID. Match both the typed message identity and
    /// the outer UUID used by attachment rows, and preserve every unrelated
    /// JSONL line verbatim.
    pub async fn remove_server_fallback_row(
        &self,
        tombstone: &lingxi_core::host::ServerFallbackTombstoneMessage,
        _display_only: bool,
    ) -> Result<usize, lingxi_core::host::FsError> {
        let row_id = serde_json::to_value(tombstone.uuid).expect("message id serializes");
        self.appended_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&tombstone.uuid);
        self.source_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&tombstone.uuid);

        let Some(fs) = self.fs.as_deref() else {
            return Ok(0);
        };
        let _journal = self.journal_lock.lock().await;
        self.recover_attachment_intent().await?;
        self.persist_message_index_high_water().await?;
        let existing = self.read_transcript().await?;
        if existing.is_empty() {
            return Ok(0);
        }

        let mut retained = String::with_capacity(existing.len());
        let mut removed = 0usize;
        for line in existing.split_inclusive('\n') {
            let json_line = line.strip_suffix('\n').unwrap_or(line);
            let matches = serde_json::from_str::<Value>(json_line)
                .ok()
                .is_some_and(|row| {
                    row.get("uuid") == Some(&row_id)
                        || row.get("message").and_then(|message| message.get("id")) == Some(&row_id)
                });
            if matches {
                removed = removed.saturating_add(1);
            } else {
                retained.push_str(line);
            }
        }
        if removed == 0 {
            return Ok(0);
        }

        let (root, relative, _) = self.attachment_paths()?;
        fs.write_file_rooted_atomic(root, relative, &retained)
            .await?;
        Ok(removed)
    }

    fn message_entry(&self, message: &ConversationMessage) -> TranscriptEntry {
        let selection = self.model_selection();
        TranscriptEntry {
            agent_id: self.agent_id,
            timestamp: SystemTime::now(),
            message: message.clone(),
            message_index: message_row_index(message),
            server_fallback_api_error_json: None,
            status: None,
            error: None,
            agent_name: self.agent_name.clone(),
            agent_type: self.agent_type.clone(),
            model: selection.model,
            model_profile: selection.model_profile,
            correlation_id: self.correlation_id.clone(),
            source_attachment: None,
            source_attachment_uuid: None,
        }
    }

    /// Dispatch the native retained-row event before persisting the row. The
    /// caller supplies prior history for tool-result origin lookup and receives
    /// accepted content edits in-place so model history and JSONL stay aligned.
    ///
    /// This variant applies only the retained-row hook/cache boundary. Streaming
    /// callers use it before scheduling a completed tool block; the row itself
    /// remains staged until the response's terminal metadata is known.
    pub async fn accept_retained(
        &self,
        message: &mut ConversationMessage,
        prior_history: &[ConversationMessage],
    ) -> Result<(), lingxi_core::host::FsError> {
        let entry = self.resolve_retained_row(message, prior_history).await?;
        *message = entry.message;
        Ok(())
    }

    /// Apply the retained-row hook to one completed Assistant block without
    /// writing JSONL. The merge projection keeps source blocks that the hook
    /// omitted, so the accepted row can be yielded to query/tool consumers and
    /// later persisted with identical content once stop metadata arrives.
    pub(crate) async fn accept_assistant_block(
        &self,
        api_block_index: u32,
        source_row: ConversationMessage,
        prior_history: &[ConversationMessage],
    ) -> Result<StagedAssistantRow, lingxi_core::host::FsError> {
        let source_tool_uses = source_tool_uses(&source_row);
        let mut accepted = source_row;
        self.accept_retained(&mut accepted, prior_history).await?;
        Ok(StagedAssistantRow {
            api_block_index,
            accepted,
            source_tool_uses,
        })
    }

    /// Persist an accepted block row after the terminal response metadata is
    /// available. Native's completed-row writer does not append a row whose
    /// stop reason is still absent.
    pub(crate) async fn persist_assistant_block(
        &self,
        row: &mut StagedAssistantRow,
        stop_reason: Option<&str>,
    ) -> Result<bool, lingxi_core::host::FsError> {
        let Some(stop_reason) = stop_reason else {
            return Ok(false);
        };
        if let ConversationMessage::Assistant {
            stop_reason: stored,
            ..
        } = &mut row.accepted
        {
            *stored = Some(stop_reason.to_string());
        } else {
            return Ok(false);
        }

        let row_id = row.accepted.id();
        {
            let mut appended_rows = self
                .appended_rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = appended_rows.get_mut(&row_id) {
                cached.message = row.accepted.clone();
            }
        }
        let transcript_entry = self.message_entry(&row.accepted);
        self.append_transcript_entry(&transcript_entry, None)
            .await?;
        Ok(true)
    }

    /// Apply the native retained-row hook/cache boundary and persist one
    /// [`TranscriptEntry`]. The caller supplies prior history for tool-result
    /// origin lookup and receives accepted content edits in-place so model
    /// history and JSONL stay aligned.
    pub async fn record_retained(
        &self,
        message: &mut ConversationMessage,
        prior_history: &[ConversationMessage],
    ) -> Result<(), lingxi_core::host::FsError> {
        let row_id = message.id();
        let entry = self.resolve_retained_row(message, prior_history).await?;
        *message = entry.message.clone();
        let selection = self.model_selection();
        let transcript_entry = TranscriptEntry {
            agent_id: self.agent_id,
            timestamp: SystemTime::now(),
            message_index: message_row_index(&entry.message),
            server_fallback_api_error_json: None,
            message: entry.message,
            status: None,
            error: None,
            agent_name: self.agent_name.clone(),
            agent_type: self.agent_type.clone(),
            model: selection.model,
            model_profile: selection.model_profile,
            correlation_id: self.correlation_id.clone(),
            source_attachment: entry
                .source_attachment
                .as_ref()
                .map(|attachment| attachment.projection.value.clone()),
            source_attachment_uuid: entry
                .source_attachment
                .as_ref()
                .map(|attachment| attachment.uuid.clone()),
        };
        self.append_transcript_entry(&transcript_entry, entry.source_attachment.as_ref())
            .await?;
        if transcript_entry.source_attachment.is_some() {
            if let Some(entry) = self
                .appended_rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&row_id)
            {
                // The attachment sidecar is consumed by the first successful
                // append for this row UUID. A later explicit write of that same
                // row may append its message again, but it must not emit the
                // already-persisted sidecar a second time.
                entry.source_attachment = None;
            }
            self.source_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&row_id);
        }
        Ok(())
    }

    async fn resolve_retained_row(
        &self,
        message: &mut ConversationMessage,
        prior_history: &[ConversationMessage],
    ) -> Result<AppendedRow, lingxi_core::host::FsError> {
        let row_id = message.id();
        if let Some(entry) = self
            .appended_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&row_id)
            .cloned()
        {
            *message = entry.message.clone();
            return Ok(entry);
        }

        let source_attachment = self
            .source_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&row_id)
            .cloned();
        let host = match &self.mod_executor {
            Some(executor) => executor.mod_host().await,
            None => None,
        }
        .filter(|host| host.has_event("session.append"));
        let cwd = self.mod_cwd.as_deref().unwrap_or_else(|| {
            self.transcript_path
                .parent()
                .unwrap_or(&self.transcript_path)
        });

        let source_attachment = match source_attachment {
            Some(source_attachment) => {
                if let Some(host) = host.as_ref() {
                    let should_dispatch = self
                        .dispatched_attachment_rows
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(source_attachment.uuid.clone());
                    let rewritten = if should_dispatch {
                        mod_append_source_attachment(host, cwd, self.agent_id, &source_attachment)
                            .await
                    } else {
                        source_attachment
                    };
                    self.source_attachments
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(row_id, rewritten.clone());
                    Some(rewritten)
                } else {
                    Some(source_attachment)
                }
            }
            None => None,
        };

        if let Some(host) = host.as_ref() {
            if host.has_event("session.append") {
                let selection = self.model_selection();
                *message = mod_append_message_row(
                    host,
                    cwd,
                    self.agent_id,
                    selection
                        .mod_model
                        .as_deref()
                        .or(selection.model.as_deref()),
                    prior_history,
                    message,
                )
                .await;
            }
        }

        let entry = AppendedRow {
            message: message.clone(),
            source_attachment,
        };
        self.appended_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(row_id, entry.clone());
        Ok(entry)
    }

    /// Persist explicit historical thinking ranges before the provider retry.
    pub async fn record_thinking_recovery(
        &self,
        messages: std::collections::HashMap<lingxi_core::types::MessageId, usize>,
    ) -> Result<(), lingxi_core::host::FsError> {
        self.record(&ConversationMessage::System { api_system: None,
            id: lingxi_core::types::MessageId::new(),
            content: serde_json::to_string(&messages).expect("thinking ranges serialize"),
            subtype: Some("thinking_stripped".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        })
        .await
    }

    /// Persist one typed attachment row with exact text units retained beside
    /// both its message and attachment payload in the transcript projection.
    pub async fn record_attachment_message(
        &self,
        message: &ConversationMessage,
        attachment: Utf16JsonProjection,
    ) -> Result<(), lingxi_core::host::FsError> {
        let entry = self.message_entry(message);
        let projection = transcript_attachment_projection(&entry, attachment)
            .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        self.append_projection(projection).await
    }

    /// Commit one admitted peer row durably before its task-owned queue may be
    /// acknowledged. A synced write intent makes partial appends recoverable;
    /// its stable message id prevents duplicate rows after sync failure/restart.
    pub async fn record_durable_attachment_once(
        &self,
        message: &ConversationMessage,
        attachment: serde_json::Value,
    ) -> Result<(), lingxi_core::host::FsError> {
        let fs = self.durable_fs()?;
        let _journal = self.journal_lock.lock().await;
        self.recover_attachment_intent().await?;
        let (root, relative, intent) = self.attachment_paths()?;
        let existing = self.read_transcript().await?;
        let entry = self.attachment_entry(message.id(), Some(message), attachment);
        let overrides = peer_attachment_overrides(&entry)?;
        for line in existing.lines().filter(|line| !line.is_empty()) {
            let decoded = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line)
                .map_err(|_| attachment_io("transcript contains an uncommitted or invalid row"))?;
            let string_overrides = decoded.string_overrides();
            let row = decoded.value;
            if row["uuid"] == entry["uuid"] || row["message"]["id"] == entry["uuid"] {
                if row["type"] != "attachment"
                    || row["agent_id"] != entry["agent_id"]
                    || row["message"] != entry["message"]
                    || row["attachment"] != entry["attachment"]
                    || string_overrides != overrides
                    || !decoded.keys.is_empty()
                {
                    return Err(attachment_io("peer transcript identity collision"));
                }
                return fs.sync_file_rooted_no_follow(root, relative).await;
            }
        }
        if !existing.is_empty() && !existing.ends_with('\n') {
            return Err(attachment_io("transcript has an unowned partial tail"));
        }
        let mut encoded = lingxi_core::types::exact_json::to_vec_with_overrides(&entry, &overrides)
            .map_err(|_| attachment_io("typed peer serialization failed"))?;
        encoded.push(b'\n');
        let pending = DurableAttachmentIntent {
            offset: u64::try_from(existing.len())
                .map_err(|_| attachment_io("transcript length overflow"))?,
            line: String::from_utf8(encoded).expect("escaped JSON is UTF-8"),
        };
        self.validate_attachment_intent(&pending)?;
        fs.write_file_rooted_atomic(
            root,
            &intent,
            &serde_json::to_string(&pending).expect("intent serialization"),
        )
        .await?;
        self.recover_attachment_intent().await
    }

    /// Some typed announcements (an empty session_context) have no model
    /// projection. Persist their payload without inventing a history message.
    pub async fn record_context_attachment(
        &self,
        id: lingxi_core::types::MessageId,
        message: Option<&ConversationMessage>,
        attachment: serde_json::Value,
    ) -> Result<(), lingxi_core::host::FsError> {
        let entry = self.attachment_entry(id, message, attachment);
        self.append_entry(&entry).await
    }

    fn attachment_entry(
        &self,
        id: lingxi_core::types::MessageId,
        message: Option<&ConversationMessage>,
        attachment: serde_json::Value,
    ) -> serde_json::Value {
        let placeholder = ConversationMessage::user_meta(id, String::new());
        let mut entry = serde_json::to_value(self.message_entry(message.unwrap_or(&placeholder)))
            .expect("transcript attachment message serialization");
        if message.is_none() {
            entry["message"] = serde_json::Value::Null;
        }
        entry["type"] = serde_json::Value::String("attachment".into());
        entry["uuid"] = serde_json::to_value(id).expect("message id serializes");
        entry["attachment"] = attachment;
        entry
    }

    /// Append a terminal lifecycle entry while retaining a normal transcript
    /// message shape for readers that replay only `message` values.
    pub async fn record_terminal(
        &self,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), lingxi_core::host::FsError> {
        let detail = error.unwrap_or(status);
        let selection = self.model_selection();
        let entry = TranscriptEntry {
            agent_id: self.agent_id,
            timestamp: SystemTime::now(),
            message: ConversationMessage::System { api_system: None,
                id: lingxi_core::types::MessageId::new(),
                content: detail.to_string(),
                subtype: Some(format!("agent_{status}")),
                compact_metadata: None,
                model_fallback: None,
                refusal_fallback: None,
            },
            message_index: None,
            server_fallback_api_error_json: None,
            status: Some(status.to_string()),
            error: error.map(str::to_string),
            agent_name: self.agent_name.clone(),
            agent_type: self.agent_type.clone(),
            model: selection.model,
            model_profile: selection.model_profile,
            correlation_id: self.correlation_id.clone(),
            source_attachment: None,
            source_attachment_uuid: None,
        };
        self.append_entry(&entry).await
    }

    async fn append_entry(
        &self,
        entry: &(impl Serialize + Sync),
    ) -> Result<(), lingxi_core::host::FsError> {
        let projection = Utf16JsonProjection::plain(
            serde_json::to_value(entry).expect("transcript serialization"),
        );
        self.append_projection(projection).await
    }

    async fn append_transcript_entry(
        &self,
        entry: &TranscriptEntry,
        source_attachment: Option<&SourceAttachment>,
    ) -> Result<(), lingxi_core::host::FsError> {
        let projection = transcript_entry_projection(entry, source_attachment)
            .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        self.append_projection(projection).await
    }

    async fn append_projection(
        &self,
        projection: Utf16JsonProjection,
    ) -> Result<(), lingxi_core::host::FsError> {
        let _journal = self.journal_lock.lock().await;
        self.append_projection_locked(projection).await
    }

    async fn append_projection_locked(
        &self,
        projection: Utf16JsonProjection,
    ) -> Result<(), lingxi_core::host::FsError> {
        let Some(fs) = &self.fs else {
            return Ok(());
        };
        self.recover_attachment_intent().await?;
        let line = format!(
            "{}\n",
            projection
                .to_json_string()
                .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?
        );
        self.persist_message_index_high_water().await?;
        let path_str = self
            .transcript_path
            .to_str()
            .expect("utf-8 transcript path");
        // A real APPEND, not read-then-rewrite. The old hack round-tripped the
        // whole file through `read_file`, whose returned view is not
        // guaranteed byte-identical to the file — concatenating onto it
        // corrupted the JSONL — and rewrote every prior line on each message,
        // making a long conversation quadratic.
        fs.append_file(path_str, &line).await
    }

    /// Read the durable stream-index high-water mark. A missing sidecar is
    /// valid only for a genuinely empty transcript; existing rows without the
    /// index metadata cannot be safely resumed without reusing deleted slots.
    pub async fn read_next_message_index(&self) -> Result<u64, lingxi_core::host::FsError> {
        let _journal = self.journal_lock.lock().await;
        let fs = self.durable_fs()?;
        let (root, _, metadata_path) = self.message_index_paths()?;
        match fs
            .read_file_rooted_no_follow_window(root, &metadata_path, None, None)
            .await
        {
            Ok(file) if !file.truncated => {
                let value: Value = serde_json::from_str(&file.content)
                    .map_err(|_| attachment_io("message-index sidecar is invalid"))?;
                value
                    .get("next_message_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| attachment_io("message-index sidecar omitted high-water"))
            }
            Ok(_) => Err(attachment_io("message-index sidecar is truncated")),
            Err(lingxi_core::host::FsError::NotFound(_)) => {
                if self.read_transcript().await?.is_empty() {
                    Ok(0)
                } else {
                    Err(attachment_io(
                        "transcript rows lack durable message indices",
                    ))
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn persist_message_index_high_water(&self) -> Result<(), lingxi_core::host::FsError> {
        let Some(next_message_index) = current_next_message_index() else {
            return Ok(());
        };
        self.persist_message_index_high_water_value(next_message_index)
            .await
    }

    async fn persist_message_index_high_water_value(
        &self,
        next_message_index: u64,
    ) -> Result<(), lingxi_core::host::FsError> {
        let fs = self.durable_fs()?;
        let (root, _, metadata_path) = self.message_index_paths()?;
        fs.write_file_rooted_atomic(
            root,
            &metadata_path,
            &serde_json::json!({"next_message_index": next_message_index}).to_string(),
        )
        .await
    }

    fn attachment_paths(
        &self,
    ) -> Result<(&std::path::Path, &std::path::Path, PathBuf), lingxi_core::host::FsError> {
        let root = self
            .transcript_path
            .parent()
            .ok_or_else(|| attachment_io("transcript has no root"))?;
        let leaf = self
            .transcript_path
            .file_name()
            .ok_or_else(|| attachment_io("transcript has no filename"))?;
        let mut intent = leaf.to_os_string();
        intent.push(".handback-intent");
        Ok((root, std::path::Path::new(leaf), PathBuf::from(intent)))
    }

    fn message_index_paths(
        &self,
    ) -> Result<(&std::path::Path, &std::path::Path, PathBuf), lingxi_core::host::FsError> {
        let root = self
            .transcript_path
            .parent()
            .ok_or_else(|| attachment_io("transcript has no root"))?;
        let leaf = self
            .transcript_path
            .file_name()
            .ok_or_else(|| attachment_io("transcript has no filename"))?;
        let mut metadata = leaf.to_os_string();
        metadata.push(".meta");
        Ok((root, std::path::Path::new(leaf), PathBuf::from(metadata)))
    }

    async fn read_transcript(&self) -> Result<String, lingxi_core::host::FsError> {
        let fs = self.durable_fs()?;
        let (root, relative, _) = self.attachment_paths()?;
        match fs
            .read_file_rooted_no_follow_window(root, relative, None, None)
            .await
        {
            Ok(file) if !file.truncated => Ok(file.content),
            Ok(_) => Err(attachment_io("transcript snapshot is truncated")),
            Err(lingxi_core::host::FsError::NotFound(_)) => Ok(String::new()),
            Err(error) => Err(error),
        }
    }

    async fn recover_attachment_intent(&self) -> Result<(), lingxi_core::host::FsError> {
        let fs = self.durable_fs()?;
        let (root, relative, intent_path) = self.attachment_paths()?;
        let intent = match fs
            .read_file_rooted_no_follow_window(root, &intent_path, None, None)
            .await
        {
            Ok(file) if !file.truncated => {
                serde_json::from_str::<DurableAttachmentIntent>(&file.content)
                    .map_err(|_| attachment_io("invalid peer append intent"))?
            }
            Ok(_) => return Err(attachment_io("truncated peer append intent")),
            Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        self.validate_attachment_intent(&intent)?;
        if intent.offset != 0 {
            let before = fs
                .read_file_rooted_byte_window_pinned(root, relative, None, intent.offset - 1, 1)
                .await?;
            if before != [b'\n'] {
                return Err(attachment_io("peer append offset is not a row boundary"));
            }
        }
        let line_bytes = intent.line.as_bytes();
        let length = u64::try_from(line_bytes.len())
            .map_err(|_| attachment_io("peer append length overflow"))?;
        let tail = match fs
            .read_file_rooted_byte_window_pinned(root, relative, None, intent.offset, length)
            .await
        {
            Ok(tail) => tail,
            Err(lingxi_core::host::FsError::NotFound(_)) if intent.offset == 0 => Vec::new(),
            Err(error) => return Err(error),
        };
        if tail == line_bytes {
            fs.sync_file_rooted_no_follow(root, relative).await?;
        } else if tail.len() < line_bytes.len() && line_bytes.starts_with(&tail) {
            if !tail.is_empty() {
                fs.truncate_file_rooted_no_follow(root, relative, intent.offset)
                    .await?;
            }
            fs.append_file_rooted_durable(root, relative, &intent.line)
                .await?;
        } else {
            return Err(attachment_io(
                "peer append tail does not match its durable intent",
            ));
        }
        fs.delete_file_rooted_no_follow(root, &intent_path).await
    }

    fn durable_fs(&self) -> Result<&dyn FileSystem, lingxi_core::host::FsError> {
        self.fs
            .as_deref()
            .ok_or_else(|| attachment_io("durable transcript filesystem is unavailable"))
    }

    fn validate_attachment_intent(
        &self,
        intent: &DurableAttachmentIntent,
    ) -> Result<(), lingxi_core::host::FsError> {
        let decoded = lingxi_core::types::exact_json::parse_exact_json(&intent.line)
            .map_err(|_| attachment_io("peer append intent is not one JSON row"))?;
        let row = decoded.value;
        if !intent.line.ends_with('\n')
            || row["type"] != "attachment"
            || row["agent_id"] != serde_json::to_value(self.agent_id).expect("actor serializes")
            || row["attachment"]["type"] != "subagent_handback"
        {
            return Err(attachment_io("peer append intent owner is invalid"));
        }
        let envelope: lingxi_core::host::handback::HandbackEnvelope =
            serde_json::from_value(row["attachment"]["envelope"].clone())
                .map_err(|_| attachment_io("peer append envelope is invalid"))?;
        if !envelope.validate()
            || !matches!(envelope.receipt.recipient, lingxi_core::host::handback::HandbackRecipient::Agent { agent_id, .. } if agent_id == self.agent_id)
            || row["uuid"]
                != serde_json::to_value(envelope.receipt.message_id).expect("receipt serializes")
            || row["message"]
                != serde_json::to_value(envelope.model_message()).expect("peer message serializes")
            || decoded.utf16_overrides != peer_attachment_overrides(&row)?
        {
            return Err(attachment_io("peer append intent projection is invalid"));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct DurableAttachmentIntent {
    offset: u64,
    line: String,
}

fn attachment_io(message: &str) -> lingxi_core::host::FsError {
    lingxi_core::host::FsError::Io(message.into())
}

fn peer_attachment_overrides(
    entry: &serde_json::Value,
) -> Result<lingxi_core::types::exact_json::Utf16Overrides, lingxi_core::host::FsError> {
    let envelope: lingxi_core::host::handback::HandbackEnvelope =
        serde_json::from_value(entry["attachment"]["envelope"].clone())
            .map_err(|_| attachment_io("peer attachment envelope is invalid"))?;
    Ok(envelope.transcript_utf16_overrides())
}

/// Consume recovery metadata from a restored worker history. The marker is a
/// transcript row, never a provider message; explicit IDs preserve fresh blocks
/// appended after earlier rejections and do not depend on row ordering.
pub(crate) fn restore_thinking_recovery(
    history: &mut Vec<ConversationMessage>,
    scope: &llm_runtime::thinking_scope::ThinkingRecoveryScope,
) {
    history.retain(|message| {
        if let ConversationMessage::System {
            content,
            subtype: Some(subtype),
            ..
        } = message
        {
            if subtype == "thinking_stripped" {
                if let Ok(ranges) = serde_json::from_str(content) {
                    scope.merge(ranges);
                }
                return false;
            }
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn append_overlay_replays_after_every_duplicate_source_tool_identity() {
        let first = ContentBlock::ToolUse { input_projection: None,
            id: lingxi_core::types::ToolUseId::from("source-a"),
            name: "Read".into(),
            input: serde_json::json!({"file_path":"a"}),
            provider_id: Some("toolu_duplicate".into()),
        };
        let second = ContentBlock::ToolUse { input_projection: None,
            id: lingxi_core::types::ToolUseId::from("source-b"),
            name: "Read".into(),
            input: serde_json::json!({"file_path":"b"}),
            provider_id: Some("toolu_duplicate".into()),
        };
        let accepted_text = ContentBlock::Text {
            text: "accepted suffix".into(),
            citations: Some(None),
        };
        let rewritten = rewrite_append_blocks(
            &[first.clone(), second.clone()],
            &[
                serde_json::json!({
                    "type":"tool_use",
                    "id":"toolu_duplicate",
                    "name":"Write",
                    "input":{"replacement":true},
                }),
                serde_json::json!({"type":"text","text":"accepted suffix"}),
            ],
            &[],
         None);
        assert_eq!(
            rewritten,
            vec![first, accepted_text.clone(), second, accepted_text]
        );
    }

    #[test]
    fn append_projection_preserves_optional_wire_fields() {
        let text = ContentBlock::Text {
            text: "answer".into(),
            citations: Some(None),
        };
        let projected_text = append_content_block(&text);
        assert!(projected_text.get("citations").is_some());
        assert_eq!(projected_text["citations"], Value::Null);

        let omitted = ContentBlock::ToolResult { content_projection: None,
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_1"),
            content: "ok".into(),
            is_error: None,
            provider_tool_use_id: None,
            content_blocks: None,
        };
        assert!(append_content_block(&omitted).get("is_error").is_none());

        let explicit_false = ContentBlock::ToolResult { content_projection: None,
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_1"),
            content: "ok".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        };
        assert_eq!(
            append_content_block(&explicit_false)["is_error"],
            Value::Bool(false)
        );

        let accepted_text = rewrite_append_blocks(
            &[],
            &[serde_json::json!({
                "type":"text",
                "text":"new answer",
                "citations":null,
            })],
            &[],
         None);
        assert!(matches!(
            accepted_text.as_slice(),
            [ContentBlock::Text {
                citations: Some(None),
                ..
            }]
        ));
        let projected_accepted_text = append_content_block(&accepted_text[0]);
        assert!(projected_accepted_text.get("citations").is_some());
        assert_eq!(projected_accepted_text["citations"], Value::Null);

        let old_error = ContentBlock::ToolResult { content_projection: None,
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_1"),
            content: "failed".into(),
            is_error: Some(true),
            provider_tool_use_id: None,
            content_blocks: None,
        };
        let omitted_error = rewrite_tool_result(
            &old_error,
            &serde_json::json!({
                "type":"tool_result",
                "content":"accepted",
            }),
         None);
        assert!(matches!(
            omitted_error,
            ContentBlock::ToolResult { is_error: None, .. }
        ));
    }

    #[test]
    fn unchanged_opaque_anthropic_text_echo_preserves_source_metadata() {
        let source = ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: serde_json::json!({
                "type":"text",
                "text":"provider answer",
                "citations":[{"type":"future_citation"}],
                "extra_native":{"kept":289},
            }),
        };
        let projected = append_content_block(&source);
        assert_eq!(
            rewrite_append_blocks(
                std::slice::from_ref(&source),
                std::slice::from_ref(&projected),
                &[],
             None),
            vec![source.clone()]
        );

        let mut changed = projected;
        changed["text"] = Value::String("replacement answer".into());
        assert_eq!(
            rewrite_append_blocks(&[source], &[changed], &[], None),
            vec![ContentBlock::Text {
                text: "replacement answer".into(),
                citations: Some(None),
            }]
        );
    }

    struct FailOnceTranscriptFs {
        fail_next_append: AtomicBool,
        append_attempts: AtomicUsize,
        body: Mutex<String>,
    }

    impl Default for FailOnceTranscriptFs {
        fn default() -> Self {
            Self {
                fail_next_append: AtomicBool::new(true),
                append_attempts: AtomicUsize::new(0),
                body: Mutex::new(String::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl FileSystem for FailOnceTranscriptFs {
        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::FsError>
        {
            Err(lingxi_core::host::FsError::NotFound(path.to_owned()))
        }

        async fn write_file(
            &self,
            _path: &str,
            content: &str,
        ) -> Result<(), lingxi_core::host::FsError> {
            *self
                .body
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = content.to_owned();
            Ok(())
        }

        fn is_within_workspace(&self, _path: &str) -> bool {
            true
        }

        async fn watch(
            &self,
            _dir: &str,
        ) -> Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = lingxi_core::host::filesystem::FileEvent> + Send>,
            >,
            lingxi_core::host::FsError,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn append_file(
            &self,
            _path: &str,
            content: &str,
        ) -> Result<(), lingxi_core::host::FsError> {
            self.append_attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail_next_append.swap(false, Ordering::SeqCst) {
                return Err(lingxi_core::host::FsError::Io(
                    "injected append failure".into(),
                ));
            }
            self.body
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_str(content);
            Ok(())
        }

        async fn truncate(&self, _path: &str, _len: u64) -> Result<(), lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }

        async fn file_mtime(
            &self,
            _path: &str,
        ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }

        async fn file_size(&self, _path: &str) -> Result<u64, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }

        async fn delete_file(&self, _path: &str) -> Result<(), lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }

        async fn symlink(
            &self,
            _target: &str,
            _link: &str,
        ) -> Result<(), lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }

        async fn flock_exclusive(
            &self,
            _path: &str,
        ) -> Result<Box<dyn lingxi_core::host::filesystem::FlockGuard>, lingxi_core::host::FsError>
        {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }

        async fn fsync(&self, _path: &str) -> Result<(), lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io(
                "not used by transcript test".into(),
            ))
        }
    }

    #[tokio::test]
    async fn model_selection_is_shared_and_commits_only_after_persistence() {
        let fs = Arc::new(FailOnceTranscriptFs::default());
        let writer = AgentTranscriptWriter::new(
            "/tmp/agent-model-selection.jsonl".into(),
            AgentId::new(),
            fs.clone(),
        )
        .with_metadata(None, None, Some("shared-model".into()), Some("a".into()));
        let cloned = writer.clone();
        assert!(
            writer
                .record_model_selection("shared-model", Some("b"))
                .await
                .is_err()
        );
        let selection = cloned.model_selection();
        assert_eq!(selection.model.as_deref(), Some("shared-model"));
        assert_eq!(selection.model_profile.as_deref(), Some("a"));

        writer
            .record_model_selection("shared-model", Some("b"))
            .await
            .unwrap();
        cloned.record_terminal("idle", None).await.unwrap();
        let body = fs.body.lock().unwrap();
        let rows: Vec<Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["type"], "model-selection");
        for row in &rows {
            assert_eq!(row["model"], "shared-model");
            assert_eq!(row["model_profile"], "b");
        }
        assert_eq!(
            cloned.model_selection().mod_model.as_deref(),
            Some("shared-model")
        );
    }

    #[tokio::test]
    async fn attachment_message_persists_message_and_attachment_utf16_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-attachment-utf16.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        );
        let mut message_units = "reminder ".encode_utf16().collect::<Vec<_>>();
        message_units.push(0xd800);
        let message = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::TextJsUtf16 {
                    text: String::from_utf16_lossy(&message_units),
                    utf16_code_units: message_units.clone(),
                    citations: None,
                },
                ContentBlock::Text {
                    text: "valid Unicode λ 😀".into(),
                    citations: None,
                },
            ],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let mut attachment_units = "context ".encode_utf16().collect::<Vec<_>>();
        attachment_units.push(0xdfff);
        let mut attachment = Utf16JsonProjection::plain(serde_json::json!({
            "type":"hook_additional_context",
            "content":[String::from_utf16_lossy(&attachment_units), "valid Unicode Ω 🧪"],
        }));
        attachment.strings.push(Utf16JsonString {
            pointer: "/content/0".into(),
            code_units: attachment_units.clone(),
        });
        attachment.validate().unwrap();

        writer
            .record_attachment_message(&message, attachment)
            .await
            .unwrap();

        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains(r"\ud800"));
        assert!(raw.contains(r"\udfff"));
        assert!(!raw.contains("utf16_code_units"));
        let row = Utf16JsonProjection::parse(raw.lines().next().unwrap()).unwrap();
        assert_eq!(row.value["type"], "attachment");
        assert_eq!(row.value["message"]["content"][0]["text"], "reminder �");
        assert_eq!(
            row.string_units("/message/content/0/text"),
            Some(message_units)
        );
        assert_eq!(
            row.value["message"]["content"][1]["text"],
            "valid Unicode λ 😀"
        );
        assert_eq!(row.value["attachment"]["content"][0], "context �");
        assert_eq!(
            row.string_units("/attachment/content/0"),
            Some(attachment_units)
        );
        assert_eq!(row.value["attachment"]["content"][1], "valid Unicode Ω 🧪");
        assert_eq!(row.strings.len(), 2);
    }

    #[tokio::test]
    async fn source_attachment_is_persisted_with_its_message_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-source.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        );
        let message = ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "<system-reminder>\ntool.call hook additional context: one\n</system-reminder>".into(),
        );
        let source = serde_json::json!({
            "type":"hook_additional_context",
            "content":["one"],
            "hookName":"tool.call",
            "toolUseID":"toolu_1-context",
            "hookEvent":"PostToolUse",
        });
        writer.register_source_attachment(&message, Utf16JsonProjection::plain(source.clone()));
        writer.record(&message).await.unwrap();
        writer.record(&message).await.unwrap();
        let entries = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<TranscriptEntry>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries[0].source_attachment, Some(source));
        assert!(entries[0].source_attachment_uuid.is_some());
        assert_eq!(entries[1].source_attachment, None);
        assert_eq!(entries[1].source_attachment_uuid, None);
    }

    #[tokio::test]
    async fn display_only_fallback_tombstone_removes_message_and_outer_uuid_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-tombstones.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        );
        scope_message_row_indexes(None, async {
            let removed_id = MessageId::new();
            let removed = ConversationMessage::Assistant { per_turn_effort: None,
                id: removed_id,
                content: vec![ContentBlock::Text {
                    text: "discarded server row".into(),
                    citations: None,
                }],
                stop_reason: None,
            };
            let retained = ConversationMessage::user_meta(MessageId::new(), "keep me".into());
            writer.record(&removed).await.unwrap();
            writer.record(&retained).await.unwrap();
            writer
                .record_context_attachment(
                    removed_id,
                    Some(&removed),
                    serde_json::json!({"type":"date","date":"2026-10-04"}),
                )
                .await
                .unwrap();

            let written = std::fs::read_to_string(&path).unwrap();
            let first_entry: TranscriptEntry =
                serde_json::from_str(written.lines().next().unwrap()).unwrap();
            assert_eq!(first_entry.message_index, Some(0));

            let tombstone = lingxi_core::host::ServerFallbackTombstoneMessage {
                uuid: removed_id,
                message_type: "assistant".into(),
                timestamp: "2026-10-04T12:00:00.000Z".into(),
                request_id: None,
                request_ref: None,
                provider_message_id: None,
                model: Some("primary-model".into()),
                stop_reason: None,
                stop_details: None,
                usage: None,
                content: vec![ContentBlock::Text {
                    text: "discarded server row".into(),
                    citations: None,
                }],
                is_api_error_message: None,
                supersedes_uuids: None,
            };
            let removed_lines = writer
                .remove_server_fallback_row(&tombstone, true)
                .await
                .unwrap();

            assert_eq!(
                removed_lines, 2,
                "both the message and attachment row use the UUID"
            );
            let remaining = std::fs::read_to_string(path).unwrap();
            assert!(remaining.contains(&retained.id().as_uuid().to_string()));
            assert!(!remaining.contains(&removed_id.as_uuid().to_string()));
            assert_eq!(remaining.lines().count(), 1);
            assert_eq!(writer.read_next_message_index().await.unwrap(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn declined_fallback_transcript_row_keeps_full_api_envelope_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-api-error.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        );
        let mut row = lingxi_core::host::ServerFallbackApiErrorRow::new(
            "declined target",
            "2026-10-04T12:00:00.000Z".into(),
        );
        row.set_refusal(
            Some("request-17".into()),
            serde_json::json!({"type":"refusal","category":"safety"}),
        );
        scope_message_row_indexes(None, async {
            writer
                .record_server_fallback_api_error_row(&row)
                .await
                .unwrap();
        })
        .await;

        let raw = std::fs::read_to_string(path).unwrap();
        let value: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(value["uuid"], serde_json::to_value(row.uuid).unwrap());
        assert_eq!(value["message_index"], 0);
        let encoded_row: lingxi_core::host::ServerFallbackApiErrorRow =
            serde_json::from_str(value["server_fallback_api_error_json"].as_str().unwrap())
                .unwrap();
        assert_eq!(encoded_row, row);
        assert_eq!(writer.read_next_message_index().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn append_failure_retry_reuses_attachment_and_row_dispatch_results() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("append-retry.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
  let calls = 0;
  on('session.append', ($, e, next) => {
    calls++;
    if (e.message.type === 'attachment') {
      return next({ ...e, message: { ...e.message,
        content: [{type:'text',text:'source edited'}] } });
    }
    if (e.message.type === 'user') {
      return next({ ...e, message: { ...e.message,
        content: e.message.content.map(block => ({...block,text:`${block.text} edited`})) } });
    }
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: String(calls) }));
}"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("append-retry", dir.path(), &module, serde_json::json!({}))
            .await
            .unwrap();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host.clone());
        let executor = Arc::new(hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(test_harness::mocks::MockHttpTransport::new()),
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        ));
        let fs = Arc::new(FailOnceTranscriptFs::default());
        let agent_id = AgentId::new();
        let writer =
            AgentTranscriptWriter::new(dir.path().join("agent-retry.jsonl"), agent_id, fs.clone())
                .with_mod_append(
                    Some(executor),
                    dir.path().to_path_buf(),
                    Some("test-model".into()),
                );
        let message = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>tool.call context</system-reminder>".into(),
        );
        writer.register_source_attachment(
            &message,
            Utf16JsonProjection::plain(serde_json::json!({
                "type":"hook_additional_context",
                "content":["source original"],
                "hookName":"tool.call",
                "toolUseID":"toolu_retry-context",
                "hookEvent":"PostToolUse",
            })),
        );
        let mut message = message;

        assert!(writer.record_retained(&mut message, &[]).await.is_err());
        assert!(message.text_content().ends_with(" edited"));
        writer.record_retained(&mut message, &[]).await.unwrap();

        assert_eq!(fs.append_attempts.load(Ordering::SeqCst), 2);
        let body = fs
            .body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut lines = body.lines();
        let entry: TranscriptEntry = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(
            lines.next().is_none(),
            "retry writes only one transcript row"
        );
        assert!(entry.message.text_content().ends_with(" edited"));
        assert!(entry.source_attachment_uuid.is_some());
        assert_eq!(
            entry.source_attachment.unwrap()["content"],
            serde_json::json!(["source edited"])
        );
        let calls = host
            .dispatch(
                "prompt.submit",
                serde_json::json!({"text":"probe"}),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        assert_eq!(calls["text"], "2", "attachment and row dispatch once each");
        assert_eq!(entry.agent_id, agent_id);
    }

    #[tokio::test]
    async fn source_attachment_append_accepts_and_persists_exact_js_utf16_text() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("append-utf16.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
  on('session.append', ($, e, next) => {
    if (e.message.type === 'attachment') {
      return next({ ...e, message: { ...e.message,
        content: [{type:'text',text:'accepted\uD800'}] } });
    }
    return next(e);
  });
}"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("append-utf16", dir.path(), &module, serde_json::json!({}))
            .await
            .unwrap();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let executor = Arc::new(hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(test_harness::mocks::MockHttpTransport::new()),
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        ));
        let path = dir.path().join("agent-append-utf16.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        )
        .with_mod_append(
            Some(executor),
            dir.path().to_path_buf(),
            Some("test-model".into()),
        );
        let message =
            ConversationMessage::user_meta(MessageId::new(), "source attachment reminder".into());
        writer.register_source_attachment(
            &message,
            Utf16JsonProjection::plain(serde_json::json!({
                "type":"hook_additional_context",
                "content":["original"],
                "hookName":"tool.call",
                "toolUseID":"toolu_utf16-context",
                "hookEvent":"PostToolUse",
            })),
        );
        let mut retained = message;
        writer.record_retained(&mut retained, &[]).await.unwrap();

        let raw = std::fs::read_to_string(path).unwrap();
        assert!(
            raw.contains(r"\ud800"),
            "expected exact UTF-16 escape in transcript row; got {raw}"
        );
        assert!(!raw.contains("utf16_code_units"));
        let row = Utf16JsonProjection::parse(raw.lines().next().unwrap()).unwrap();
        assert_eq!(row.value["source_attachment"]["content"][0], "accepted�");
        let mut expected = "accepted".encode_utf16().collect::<Vec<_>>();
        expected.push(0xd800);
        assert_eq!(
            row.string_units("/source_attachment/content/0"),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn source_attachment_append_rejects_nontext_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("append-nontext.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
  on('session.append', ($, e, next) => {
    if (e.message.type === 'attachment') {
      return next({ ...e, message: { ...e.message,
        content: [{type:'image',text:'rejected'}] } });
    }
    return next(e);
  });
}"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("append-nontext", dir.path(), &module, serde_json::json!({}))
            .await
            .unwrap();
        assert!(host.has_event("session.append"));
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let executor = Arc::new(hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(test_harness::mocks::MockHttpTransport::new()),
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        ));
        let path = dir.path().join("agent-append-nontext.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        )
        .with_mod_append(
            Some(executor),
            dir.path().to_path_buf(),
            Some("test-model".into()),
        );
        let message =
            ConversationMessage::user_meta(MessageId::new(), "source attachment reminder".into());
        writer.register_source_attachment(
            &message,
            Utf16JsonProjection::plain(serde_json::json!({
                "type":"hook_additional_context",
                "content":["original"],
                "hookName":"tool.call",
                "toolUseID":"toolu_nontext-context",
                "hookEvent":"PostToolUse",
            })),
        );
        let mut retained = message;
        writer.record_retained(&mut retained, &[]).await.unwrap();

        let raw = std::fs::read_to_string(path).unwrap();
        let row = Utf16JsonProjection::parse(raw.lines().next().unwrap()).unwrap();
        assert_eq!(
            row.value["source_attachment"]["content"],
            serde_json::json!(["original"]),
            "non-text session.append content must preserve the original attachment"
        );
        assert!(!raw.contains("rejected"));
    }

    #[tokio::test]
    async fn worker_thinking_recovery_round_trips_ranges_and_preserves_fresh_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-recovery.jsonl");
        let writer = AgentTranscriptWriter::new(
            path.clone(),
            AgentId::new(),
            Arc::new(platform_posix::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        );
        let assistant = |id, text: &str| ConversationMessage::Assistant { per_turn_effort: None,
            id,
            content: vec![
                lingxi_core::types::ContentBlock::Thinking {
                    thinking: "keep-prefix".into(),
                    signature: Some("valid".into()),
                },
                lingxi_core::types::ContentBlock::Thinking {
                    thinking: text.into(),
                    signature: Some("sig".into()),
                },
                lingxi_core::types::ContentBlock::Text {
                    text: "answer".into(),
                    citations: None,
                },
            ],
            stop_reason: Some("end_turn".into()),
        };
        let old = assistant(lingxi_core::types::MessageId::new(), "rejected");
        let fresh = assistant(lingxi_core::types::MessageId::new(), "fresh");
        writer.record(&old).await.unwrap();
        writer
            .record_thinking_recovery([(old.id(), 1)].into_iter().collect())
            .await
            .unwrap();
        writer.record(&fresh).await.unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        let mut history: Vec<_> = body
            .lines()
            .map(|line| {
                serde_json::from_str::<TranscriptEntry>(line)
                    .unwrap()
                    .message
            })
            .collect();
        let scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
        restore_thinking_recovery(&mut history, &scope);
        assert_eq!(history.len(), 2);
        assert_eq!(scope.messages().get(&old.id()), Some(&1));
        assert!(!scope.messages().contains_key(&fresh.id()));
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut history,
            &scope.messages(),
        );
        let ConversationMessage::Assistant { content, .. } = &history[0] else {
            panic!("assistant")
        };
        assert!(
            matches!(&content[0], lingxi_core::types::ContentBlock::Thinking { thinking, .. } if thinking == "keep-prefix")
        );
        assert_eq!(content.len(), 2);
        assert_eq!(history[1], fresh);
    }
}

#[cfg(test)]
mod rich_append_projection_tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    fn worker_frame(message: &ConversationMessage) -> hooks::mods::ModUtf16ValueProjection {
        let message = append_exact_message(message).unwrap();
        let mut frame = Utf16JsonProjection::plain(serde_json::json!({"message":message.value}));
        frame.set_pointer("/message", message).unwrap();
        hooks::mods::ModUtf16ValueProjection::from_core_projection(frame).unwrap()
    }

    #[test]
    fn source_tool_input_survives_worker_key_namespace_and_detects_key_unit_change() {
        let input = Utf16JsonProjection::parse(r#"{"\ud800":"\udfff"}"#).unwrap();
        let original = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(), stop_reason: Some("tool_use".into()),
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::from("toolu_exact"), name: "Echo".into(),
                input: input.value.clone(), input_projection: Some(input.clone()), provider_id: None,
            }],
        };
        let forwarded = worker_frame(&original);
        let source = forwarded.clone().into_core_projection().unwrap();
        let rewritten = rewrite_append_message(&original, &source.value["message"], &forwarded.strings, Some(&source)).unwrap();
        assert!(append_projection_matches(&rewritten, &forwarded.value["message"], &forwarded.strings, &forwarded.keys));
        assert_eq!(append_exact_message(&rewritten).unwrap().subprojection("/content/0/input").unwrap().to_json_string().unwrap(), input.to_json_string().unwrap());
        let mut tampered = forwarded;
        tampered.keys[0].code_units = vec![0xd801];
        assert!(!append_projection_matches(&rewritten, &tampered.value["message"], &tampered.strings, &tampered.keys));
    }

    #[test]
    fn accepted_result_retains_exact_source_keys_when_append_repositions_blocks() {
        let data = Utf16JsonProjection::parse(r#"[{"type":"text","text":"\udc00","\udfff":"\ud800"}]"#).unwrap();
        let original = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(), is_meta: false, is_compact_summary: false,
            is_visible_in_transcript_only: false,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::from("toolu_exact"), content: "\u{fffd}".into(),
                is_error: Some(false), provider_tool_use_id: None,
                content_blocks: Some(data.value.as_array().unwrap().clone()), content_projection: Some(data.clone()),
            }],
        };
        let mut omitted = append_message_projection(&original);
        omitted["content"][0].as_object_mut().unwrap().remove("content");
        let retained = rewrite_append_message(&original, &omitted, &[], None).unwrap();
        assert_eq!(append_exact_message(&retained).unwrap().subprojection("/content/0/content").unwrap().to_json_string().unwrap(), data.to_json_string().unwrap());
        let mut incoming = append_message_projection(&original);
        incoming["content"].as_array_mut().unwrap().insert(0, serde_json::json!({"type":"text","text":"added"}));
        let mut frame = Utf16JsonProjection::plain(serde_json::json!({"message":incoming}));
        frame.set_pointer("/message/content/1/content", data.clone()).unwrap();
        let forwarded = hooks::mods::ModUtf16ValueProjection::from_core_projection(frame).unwrap();
        let source = forwarded.clone().into_core_projection().unwrap();
        let rewritten = rewrite_append_message(&original, &source.value["message"], &forwarded.strings, Some(&source)).unwrap();
        let result = append_exact_message(&rewritten).unwrap().subprojection("/content/0/content").unwrap();
        assert_eq!(result.to_json_string().unwrap(), data.to_json_string().unwrap());
        assert_eq!(append_message_projection(&rewritten)["content"][1]["text"], "added");
        let accepted = worker_frame(&rewritten);
        assert!(append_projection_matches(&rewritten, &accepted.value["message"], &accepted.strings, &accepted.keys));
    }
}
