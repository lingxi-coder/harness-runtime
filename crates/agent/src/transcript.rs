//! Per-agent transcript writer.
//!
//! [`AgentTranscriptWriter`] appends one JSON line per
//! [`ConversationMessage`] to a transcript file under the agent's transcript
//! subdir.

use hooks::mods::{ModError, ModHost};
use lingxi_core::host::FileSystem;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, AgentId, ContentBlock, ConversationMessage, MessageId,
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
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
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

pub(crate) async fn persist_current_message_index_high_water(
) -> Result<(), lingxi_core::host::FsError> {
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
    /// Concrete provider-local model used by this agent.
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
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_attachment: Option<Utf16JsonProjection>,
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
    let serialized = serde_json::to_string(entry)?;
    let mut projection = Utf16JsonProjection::parse(&serialized)?;
    project_message_utf16(&mut projection, &entry.message, "/message");
    if let Some(attachment) = source_attachment {
        projection.set_field("source_attachment", attachment.projection.clone())?;
    }
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
            let ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } = block
            else {
                continue;
            };
            let block_pointer = format!("{message_pointer}/content/{index}");
            let mut fields = vec![
                ("type".to_owned(), Utf16JsonProjection::from("text")),
                (
                    "text".to_owned(),
                    Utf16JsonProjection::root_string(
                        String::from_utf16_lossy(utf16_code_units),
                        utf16_code_units.clone(),
                    )
                    .expect("text display derives from source units"),
                ),
            ];
            if let ContentBlock::TextJsUtf16 { citations, .. } = block {
                if let Some(citations) = citations {
                    fields.push((
                        "citations".into(),
                        citations.clone().unwrap_or_else(source_null),
                    ));
                }
            }
            let replacement = Utf16JsonProjection::object(fields)
                .expect("generated transcript text block is an object");
            projection
                .replace_pointer_projection(&block_pointer, replacement)
                .expect("serialized message contains the source text block");
        }
    }
}

fn transcript_attachment_projection(
    entry: &TranscriptEntry,
    attachment: Utf16JsonProjection,
) -> Result<Utf16JsonProjection, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    attachment.validate()?;
    let mut projection = transcript_entry_projection(entry, None)?;
    projection.set_field("type", "attachment".into())?;
    projection.set_field("uuid", entry.message.id().as_uuid().to_string().into())?;
    projection.set_field("attachment", attachment)?;
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

fn source_object<const N: usize>(fields: [(&str, Utf16JsonProjection); N]) -> Utf16JsonProjection {
    Utf16JsonProjection::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .expect("generated field names form an object")
}

fn source_array_items(source: &Utf16JsonProjection) -> Option<Vec<Utf16JsonProjection>> {
    (0..source.array_len("")?)
        .map(|index| source.subprojection(&format!("/{index}")).ok())
        .collect()
}

fn source_null() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

fn source_from_value(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("test JSON value is valid")
}

fn append_content_block(block: &ContentBlock) -> Utf16JsonProjection {
    use ContentBlock;
    match block {
        ContentBlock::Text { text, citations }
        | ContentBlock::TextJsUtf16 {
            text, citations, ..
        } => {
            let text = match block {
                ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => Utf16JsonProjection::root_string(
                    String::from_utf16_lossy(utf16_code_units),
                    utf16_code_units.clone(),
                )
                .expect("text display derives from source units"),
                _ => text.into(),
            };
            let mut value = source_object([("type", "text".into()), ("text", text)]);
            if let Some(citations) = citations {
                value
                    .set_field("citations", citations.clone().unwrap_or_else(source_null))
                    .unwrap();
            }
            value
        }
        ContentBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
        } => source_object([
            ("type", "tool_use".into()),
            (
                "id",
                provider_id.as_deref().unwrap_or_else(|| id.as_str()).into(),
            ),
            ("name", name.into()),
            ("input", input.clone()),
        ]),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            provider_tool_use_id,
            content_blocks,
        } => {
            let mut value = source_object([
                ("type", "tool_result".into()),
                (
                    "tool_use_id",
                    provider_tool_use_id
                        .as_deref()
                        .unwrap_or_else(|| tool_use_id.as_str())
                        .into(),
                ),
                (
                    "content",
                    content_blocks.as_ref().unwrap_or(content).clone(),
                ),
            ]);
            if let Some(is_error) = is_error {
                value
                    .set_field(
                        "is_error",
                        Utf16JsonProjection::parse(if *is_error { "true" } else { "false" })
                            .unwrap(),
                    )
                    .unwrap();
            }
            value
        }
        ContentBlock::ProviderContent { value, .. } => value.clone(),
        other => Utf16JsonProjection::parse(
            &serde_json::to_string(other).expect("content block serializes"),
        )
        .expect("serialized block is valid JSON"),
    }
}

fn append_message_projection(message: &ConversationMessage) -> Utf16JsonProjection {
    let (kind, name, role, is_meta, content) = match message {
        ConversationMessage::User {
            content, is_meta, ..
        } => (
            "user",
            None,
            Some("user"),
            *is_meta,
            content.iter().map(append_content_block).collect(),
        ),
        ConversationMessage::Assistant { content, .. } => (
            "assistant",
            None,
            Some("assistant"),
            false,
            content.iter().map(append_content_block).collect(),
        ),
        ConversationMessage::System {
            content, subtype, ..
        } => (
            "system",
            subtype.as_deref(),
            None,
            false,
            if content
                .string_units("")
                .is_none_or(|units| units.is_empty())
            {
                Vec::new()
            } else {
                vec![source_object([
                    ("type", "text".into()),
                    ("text", content.into()),
                ])]
            },
        ),
    };
    let mut result = source_object([
        ("type", Utf16JsonProjection::from(kind)),
        ("content", Utf16JsonProjection::array(content).unwrap()),
    ]);
    if let Some(name) = name {
        result
            .set_field("name", Utf16JsonProjection::from(name))
            .unwrap();
    }
    if let Some(role) = role {
        result
            .set_field("role", Utf16JsonProjection::from(role))
            .unwrap();
    }
    if is_meta {
        result
            .set_field("isMeta", Utf16JsonProjection::parse("true").unwrap())
            .unwrap();
    }
    result
}

fn append_block_matches(original: &ContentBlock, incoming: &Utf16JsonProjection) -> bool {
    append_content_block(original).semantically_eq(incoming)
}

fn append_new_text_block(incoming: &Utf16JsonProjection) -> Option<ContentBlock> {
    let units = incoming.string_units("/text")?;
    Some(match String::from_utf16(&units) {
        Ok(text) => ContentBlock::Text {
            text,
            citations: Some(None),
        },
        Err(_) => ContentBlock::TextJsUtf16 {
            text: String::from_utf16_lossy(&units),
            utf16_code_units: units,
            citations: Some(None),
        },
    })
}

fn append_projection_matches(expected: &ConversationMessage, actual: &Utf16JsonProjection) -> bool {
    append_message_projection(expected).semantically_eq(actual)
}

fn rewrite_append_tool_result_blocks(
    original: Option<&Utf16JsonProjection>,
    incoming: &[Utf16JsonProjection],
) -> Vec<Utf16JsonProjection> {
    let original = original.and_then(source_array_items).unwrap_or_default();
    let mut used = vec![false; original.len()];
    incoming
        .iter()
        .filter_map(|block| {
            if let Some((index, old)) = original
                .iter()
                .enumerate()
                .find(|(index, old)| !used[*index] && old.semantically_eq(block))
            {
                used[index] = true;
                return Some(old.clone());
            }
            if block.string_at("/type") == Some("text")
                && block
                    .string_at("/text")
                    .is_some_and(|text| !text.is_empty())
            {
                return Some(source_object([
                    ("type", "text".into()),
                    ("text", block.subprojection("/text").unwrap()),
                    ("citations", source_null()),
                ]));
            }
            None
        })
        .collect()
}

fn rewrite_append_tool_result(
    original: &ContentBlock,
    incoming: &Utf16JsonProjection,
) -> ContentBlock {
    use ContentBlock;
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        provider_tool_use_id,
        content_blocks,
        ..
    } = original
    else {
        return original.clone();
    };
    let valid_content = incoming.get_property("content").is_none_or(|value| {
        value.as_str().is_some()
            || value.array_len().is_some_and(|len| {
                (0..len).all(|index| {
                    value
                        .get_index(index)
                        .and_then(|block| block.get_property("type"))
                        .and_then(|kind| kind.as_str())
                        .is_some()
                })
            })
    });
    if !valid_content
        || incoming
            .get_property("is_error")
            .is_some_and(|value| value.as_bool().is_none())
    {
        return original.clone();
    }
    let mut next_content = content.clone();
    let mut next_content_blocks = content_blocks.clone();
    if let Ok(value) = incoming.subprojection("/content") {
        if value.string_at("").is_some() {
            next_content = value;
            next_content_blocks = None;
        } else if let Some(blocks) = source_array_items(&value) {
            if content_blocks
                .as_ref()
                .is_some_and(|old| old.semantically_eq(&value))
            {
                next_content_blocks = Some(value);
            } else {
                let blocks = rewrite_append_tool_result_blocks(content_blocks.as_ref(), &blocks);
                let mut units = Vec::new();
                let mut first = true;
                for block in &blocks {
                    if let Some(text) = block.string_units("/text") {
                        if !first {
                            units.push(10);
                        }
                        first = false;
                        units.extend(text);
                    }
                }
                next_content =
                    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                        .unwrap();
                next_content_blocks = Some(Utf16JsonProjection::array(blocks).unwrap());
            }
        }
    }
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.clone(),
        content: next_content,
        is_error: incoming
            .get_property("is_error")
            .and_then(|value| value.as_bool()),
        provider_tool_use_id: provider_tool_use_id.clone(),
        content_blocks: next_content_blocks,
    }
}

fn append_identity_key(
    kind: &str,
    tool_identity: Option<&str>,
    counts: &mut std::collections::HashMap<String, usize>,
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
    let key = format!("{kind}#{}", *index);
    *index += 1;
    Some(key)
}

fn rewrite_append_blocks(
    original: &[ContentBlock],
    incoming: &[Utf16JsonProjection],
) -> Vec<ContentBlock> {
    use ContentBlock;

    let mut source_type_counts = std::collections::HashMap::new();
    let source_keys = original
        .iter()
        .map(|block| {
            let projection = append_content_block(block);
            let kind = projection.string_at("/type")?;
            let tool_identity = match block {
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
            append_identity_key(kind, tool_identity, &mut source_type_counts)
        })
        .collect::<Vec<_>>();
    let source_key_set = source_keys
        .iter()
        .filter_map(Clone::clone)
        .collect::<std::collections::HashSet<_>>();
    let source_tool_result_keys = original
        .iter()
        .zip(&source_keys)
        .filter_map(|(block, key)| {
            matches!(block, ContentBlock::ToolResult { .. })
                .then(|| key.clone())
                .flatten()
        })
        .collect::<Vec<_>>();
    let unbound_tool_result_count = incoming
        .iter()
        .filter(|block| {
            block.string_at("/type") == Some("tool_result")
                && block.string_at("/tool_use_id").is_none()
        })
        .count();
    let can_bind_unidentified_tool_results =
        unbound_tool_result_count == source_tool_result_keys.len();
    let mut unbound_tool_result_index = 0;
    let mut incoming_type_counts = std::collections::HashMap::new();
    let mut raised_source_blocks = vec![false; original.len()];
    let mut active_anchor = None::<String>;
    let mut overlays = std::collections::HashMap::<Option<String>, Vec<ContentBlock>>::new();
    let mut tool_result_overlays = std::collections::HashMap::<String, Utf16JsonProjection>::new();

    for block in incoming {
        let Some(kind) = block.string_at("/type") else {
            continue;
        };
        if matches!(kind, "text" | "image" | "document") {
            if let Some((index, old)) = original.iter().enumerate().find(|(index, old)| {
                !raised_source_blocks[*index]
                    && source_keys[*index].is_none()
                    && append_block_matches(old, block)
            }) {
                raised_source_blocks[index] = true;
                overlays
                    .entry(active_anchor.clone())
                    .or_default()
                    .push(old.clone());
            } else if kind == "text" {
                if let Some(text) = append_new_text_block(block).filter(|text| match text {
                    ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                        !text.is_empty()
                    }
                    _ => false,
                }) {
                    overlays
                        .entry(active_anchor.clone())
                        .or_default()
                        .push(text);
                }
            }
            continue;
        }

        let tool_identity = match kind {
            "tool_use" => block.string_at("/id"),
            "tool_result" => block.string_at("/tool_use_id"),
            _ => None,
        };
        let generated_key = append_identity_key(kind, tool_identity, &mut incoming_type_counts);
        let identity_key = if kind == "tool_result" && tool_identity.is_none() {
            let key = can_bind_unidentified_tool_results
                .then(|| {
                    source_tool_result_keys
                        .get(unbound_tool_result_index)
                        .cloned()
                })
                .flatten();
            unbound_tool_result_index += 1;
            key
        } else {
            generated_key
        };
        let Some(identity_key) = identity_key.filter(|key| source_key_set.contains(key)) else {
            continue;
        };
        active_anchor = Some(identity_key.clone());
        if kind == "tool_result" {
            tool_result_overlays.insert(identity_key, block.clone());
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
                (ContentBlock::ToolResult { .. }, Some(incoming)) => {
                    rewrite_append_tool_result(&original[index], incoming)
                }
                _ => original[index].clone(),
            };
            rewritten.push(block);
            rewritten.extend(
                overlays
                    // Native q reads `a.get(key)` at every source occurrence.
                    // Duplicate ToolUse ids therefore replay the same anchored
                    // text after each matching source block; only the unkeyed
                    // sentinel is consumed once above.
                    .get(&Some(identity_key.clone()))
                    .cloned()
                    .unwrap_or_default(),
            );
        }
    }
    rewritten
}

fn rewrite_append_message(
    original: &ConversationMessage,
    incoming: &Utf16JsonProjection,
) -> Result<ConversationMessage, String> {
    let Some(content) = incoming
        .subprojection("/content")
        .ok()
        .and_then(|content| source_array_items(&content))
    else {
        return Err("session.append message.content must be an array".into());
    };
    Ok(match original {
        ConversationMessage::User {
            api_message_override,
            id,
            content: old_content,
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
            turn_companion,
            usage_limit_note,
            origin,
            turn_origin,
            interrupted_by_shutdown,
            refused_message_id,
        } => ConversationMessage::User {
            api_message_override: api_message_override.clone(),
            id: *id,
            content: rewrite_append_blocks(old_content, &content),
            is_meta: *is_meta,
            is_compact_summary: *is_compact_summary,
            is_visible_in_transcript_only: *is_visible_in_transcript_only,
            turn_companion: *turn_companion,
            usage_limit_note: *usage_limit_note,
            origin: origin.clone(),
            turn_origin: turn_origin.clone(),
            interrupted_by_shutdown: *interrupted_by_shutdown,
            refused_message_id: refused_message_id.clone(),
        },
        ConversationMessage::Assistant {
            id,
            content: old_content,
            per_turn_effort,
            stop_reason,
            resumable,
        } => ConversationMessage::Assistant {
            id: *id,
            content: rewrite_append_blocks(old_content, &content),
            per_turn_effort: per_turn_effort.clone(),
            stop_reason: stop_reason.clone(),
            resumable: resumable.clone(),
        },
        ConversationMessage::System {
            id,
            subtype,
            compact_metadata,
            api_system,
            model_fallback,
            refusal_fallback,
            ..
        } => {
            let mut replacement_units = Vec::new();
            for block in content
                .iter()
                .filter(|block| block.string_at("/type") == Some("text"))
            {
                let Some(text_units) = block.string_units("/text") else {
                    continue;
                };
                if !replacement_units.is_empty() {
                    replacement_units.push(b'\n' as u16);
                }
                replacement_units.extend(text_units);
            }
            ConversationMessage::System {
                id: *id,
                content: Utf16JsonProjection::root_string(
                    String::from_utf16_lossy(&replacement_units),
                    replacement_units,
                )
                .expect("rewritten system text derives from exact source units"),
                subtype: subtype.clone(),
                compact_metadata: compact_metadata.clone(),
                api_system: api_system.clone(),
                model_fallback: model_fallback.clone(),
                refusal_fallback: refusal_fallback.clone(),
            }
        }
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
) -> Option<(Utf16JsonProjection, String, Utf16JsonProjection)> {
    let source = &attachment.projection;
    let kind = source.string_at("/type")?;
    let content_len = source.array_len("/content")?;
    let mut blocks = Vec::with_capacity(content_len);
    for index in 0..content_len {
        let pointer = format!("/content/{index}");
        let units = source.string_units(&pointer)?;
        let text =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).ok()?;
        blocks.push(source_object([("type", "text".into()), ("text", text)]));
    }
    let door = if matches!(kind, "queued_command" | "poll_events" | "teammate_mailbox") {
        "delivery"
    } else if kind.starts_with("hook_") || kind == "async_hook_response" {
        "hook-context"
    } else {
        "attachment"
    };
    let origin = if kind == "hook_additional_context" {
        match source.string_at("/hookName") {
            Some(event @ ("prompt.submit" | "tool.call")) => {
                source_object([("kind", "plugin".into()), ("event", event.into())])
            }
            _ => source_object([
                ("kind", "hook".into()),
                (
                    "event",
                    source
                        .subprojection("/hookEvent")
                        .unwrap_or_else(|_| source_null()),
                ),
            ]),
        }
    } else if source.subprojection("/hookEvent").is_ok() {
        source_object([
            ("kind", "hook".into()),
            ("event", source.subprojection("/hookEvent").ok()?),
        ])
    } else {
        source_object([("kind", "engine".into())])
    };
    let message = source_object([
        ("type", "attachment".into()),
        ("name", kind.into()),
        ("content", Utf16JsonProjection::array(blocks).ok()?),
    ]);
    Some((message, door.into(), origin))
}

async fn dispatch_append_event<F, Fut>(
    host: &Arc<ModHost>,
    session: Option<Arc<dyn hooks::mods::ModSessionContext>>,
    cwd: &std::path::Path,
    input: Utf16JsonProjection,
    core: F,
) -> Result<hooks::mods::ModDispatchOutcome, ModError>
where
    F: FnMut(Option<Utf16JsonProjection>) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
{
    if let Some(session) = session {
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
                async move { session.emit_mod_log(&plugin, text).await }
            },
            move |plugin, text, timeout_ms| {
                let session = toast_session.clone();
                async move { session.emit_mod_toast(&plugin, text, timeout_ms).await }
            },
            move |plugin, text| {
                let session = status_session.clone();
                async move { session.emit_mod_status(&plugin, text).await }
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
    session: Option<Arc<dyn hooks::mods::ModSessionContext>>,
    cwd: &std::path::Path,
    agent_id: AgentId,
    model: Option<&str>,
    prior_history: &[ConversationMessage],
    original: &ConversationMessage,
) -> ConversationMessage {
    let uuid = original.id().as_uuid().to_string();
    let (door, origin) = append_door_origin(original, prior_history, model);
    let input = source_object([
        ("message", append_message_projection(original)),
        ("door", door.into()),
        (
            "origin",
            Utf16JsonProjection::from_serde_value(&origin)
                .expect("generated append origin is valid JSON"),
        ),
        ("uuid", uuid.clone().into()),
        ("agentId", agent_id.as_uuid().to_string().into()),
    ]);
    let core_input = input.clone();
    let core_uuid = uuid.clone();
    let core_original = original.clone();
    let applied = Arc::new(Mutex::new(
        None::<(ConversationMessage, Utf16JsonProjection)>,
    ));
    let applied_by_core = applied.clone();
    let result = dispatch_append_event(host, session, cwd, input, move |forwarded| {
        let core_input = core_input.clone();
        let core_uuid = core_uuid.clone();
        let core_original = core_original.clone();
        let applied = applied_by_core.clone();
        async move {
            let forwarded =
                forwarded.ok_or_else(|| ModError::Hook("session.append needs an event".into()))?;
            for key in ["door", "origin", "uuid", "agentId"] {
                if let Ok(value) = forwarded.subprojection(&format!("/{key}")) {
                    if core_input
                        .subprojection(&format!("/{key}"))
                        .map_or(true, |original| !original.semantically_eq(&value))
                    {
                        return Err(ModError::Hook(format!("session.append {key} is pinned")));
                    }
                }
            }
            let incoming = forwarded
                .subprojection("/message")
                .map_err(|_| ModError::Hook("session.append needs message.content".into()))?;
            let original_projection = append_message_projection(&core_original);
            for key in ["type", "name", "role", "isMeta"] {
                if let Ok(value) = incoming.subprojection(&format!("/{key}")) {
                    if original_projection
                        .subprojection(&format!("/{key}"))
                        .map_or(true, |original| !original.semantically_eq(&value))
                    {
                        return Err(ModError::Hook(format!(
                            "session.append message.{key} is pinned"
                        )));
                    }
                }
            }
            let rewritten =
                rewrite_append_message(&core_original, &incoming).map_err(ModError::Hook)?;
            let projected = append_message_projection(&rewritten);
            *applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((rewritten, projected.clone()));
            Ok(Some(source_object([
                ("message", projected),
                ("uuid", core_uuid.into()),
            ])))
        }
    })
    .await;
    match result {
        Ok(outcome) => {
            let accepted = applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            match accepted {
                Some((rewritten, projected))
                    if outcome.result_projection.string_at("/uuid") == Some(uuid.as_str())
                        && outcome
                            .result_projection
                            .subprojection("/message")
                            .is_ok_and(|message| projected.semantically_eq(&message)) =>
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
    session: Option<Arc<dyn hooks::mods::ModSessionContext>>,
    cwd: &std::path::Path,
    agent_id: AgentId,
    original: &SourceAttachment,
) -> SourceAttachment {
    let Some((message, door, origin)) = source_attachment_projection(original) else {
        return original.clone();
    };
    let uuid = original.uuid.clone();
    let input = source_object([
        ("message", message),
        ("door", door.into()),
        ("origin", origin),
        ("uuid", uuid.clone().into()),
        ("agentId", agent_id.as_uuid().to_string().into()),
    ]);
    let core_input = input.clone();
    let core_original = original.clone();
    let core_message = input
        .subprojection("/message")
        .expect("generated message exists");
    let applied = Arc::new(Mutex::new(None::<(SourceAttachment, Utf16JsonProjection)>));
    let applied_by_core = applied.clone();
    let result = dispatch_append_event(host, session, cwd, input, move |forwarded| {
        let core_input = core_input.clone();
        let core_original = core_original.clone();
        let core_message = core_message.clone();
        let applied = applied_by_core.clone();
        async move {
            let forwarded =
                forwarded.ok_or_else(|| ModError::Hook("session.append needs an event".into()))?;
            for key in ["door", "origin", "uuid", "agentId"] {
                if let Ok(value) = forwarded.subprojection(&format!("/{key}")) {
                    if core_input
                        .subprojection(&format!("/{key}"))
                        .map_or(true, |old| !old.semantically_eq(&value))
                    {
                        return Err(ModError::Hook(format!("session.append {key} is pinned")));
                    }
                }
            }
            let incoming = forwarded
                .subprojection("/message")
                .map_err(|_| ModError::Hook("session.append needs message.content".into()))?;
            for key in ["type", "name"] {
                if let Ok(value) = incoming.subprojection(&format!("/{key}")) {
                    if core_message
                        .subprojection(&format!("/{key}"))
                        .map_or(true, |old| !old.semantically_eq(&value))
                    {
                        return Err(ModError::Hook(format!(
                            "session.append message.{key} is pinned"
                        )));
                    }
                }
            }
            let blocks = incoming
                .subprojection("/content")
                .ok()
                .and_then(|content| source_array_items(&content))
                .ok_or_else(|| {
                    ModError::Hook("session.append message.content must be an array".into())
                })?;
            let texts = blocks
                .into_iter()
                .map(|block| {
                    if block.string_at("/type") != Some("text") {
                        return Err(ModError::Hook(
                            "session.append attachment content must remain text".into(),
                        ));
                    }
                    block.subprojection("/text").map_err(|_| {
                        ModError::Hook("session.append attachment text is invalid".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut rewritten = core_original.clone();
            rewritten
                .projection
                .replace_pointer_projection(
                    "/content",
                    Utf16JsonProjection::array(texts).map_err(|error| {
                        ModError::Hook(format!(
                            "session.append attachment projection is invalid: {error}"
                        ))
                    })?,
                )
                .map_err(|error| {
                    ModError::Hook(format!(
                        "session.append attachment projection is invalid: {error}"
                    ))
                })?;
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
            Ok(Some(source_object([
                ("message", projected),
                ("uuid", core_original.uuid.clone().into()),
            ])))
        }
    })
    .await;
    match result {
        Ok(outcome) => {
            let accepted = applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            match accepted {
                Some((rewritten, projected))
                    if outcome.result_projection.string_at("/uuid") == Some(uuid.as_str())
                        && outcome
                            .result_projection
                            .subprojection("/message")
                            .is_ok_and(|message| projected.semantically_eq(&message)) =>
                {
                    rewritten
                }
                _ => {
                    tracing::warn!(%uuid, "child attachment session.append Mod did not accept its projection; preserving the original sidecar");
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
    correlation_id: Option<String>,
    model_selection: Arc<Mutex<TranscriptModelSelection>>,
    journal_lock: Arc<tokio::sync::Mutex<()>>,
    source_attachments: Arc<Mutex<HashMap<MessageId, SourceAttachment>>>,
    mod_executor: Option<Arc<hooks::HookExecutorImpl>>,
    mod_cwd: Option<PathBuf>,
    mod_session_origin: Option<Arc<dyn lingxi_core::host::subagent_spawn::SessionOriginCapability>>,
    mod_require_session_origin: bool,
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
            correlation_id: None,
            model_selection: Arc::new(Mutex::new(TranscriptModelSelection::default())),
            journal_lock: Arc::new(tokio::sync::Mutex::new(())),
            source_attachments: Arc::new(Mutex::new(HashMap::new())),
            mod_executor: None,
            mod_cwd: None,
            mod_session_origin: None,
            mod_require_session_origin: false,
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
            correlation_id: None,
            model_selection: Arc::new(Mutex::new(TranscriptModelSelection::default())),
            journal_lock: Arc::new(tokio::sync::Mutex::new(())),
            source_attachments: Arc::new(Mutex::new(HashMap::new())),
            mod_executor: None,
            mod_cwd: None,
            mod_session_origin: None,
            mod_require_session_origin: false,
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

    /// Retain the child launch's immutable Mod origin without holding a
    /// session work lease while the transcript writer is idle.
    #[must_use]
    pub fn with_mod_session_origin(
        mut self,
        origin: Option<Arc<dyn lingxi_core::host::subagent_spawn::SessionOriginCapability>>,
        require_origin: bool,
    ) -> Self {
        self.mod_session_origin = origin;
        self.mod_require_session_origin = require_origin;
        self
    }

    fn model_selection(&self) -> TranscriptModelSelection {
        self.model_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Persist a committed logical selection even if the child parks before
    /// another ordinary message is written. Cloned writers share the state.
    pub(crate) async fn record_model_selection(
        &self,
        model: &str,
        model_profile: Option<&str>,
    ) -> Result<(), lingxi_core::host::FsError> {
        let _journal = self.journal_lock.lock().await;
        let projection = Utf16JsonProjection::from_serde_value(&serde_json::json!({
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
        }))
        .map_err(|_| attachment_io("model selection transcript row is invalid"))?;
        self.append_projection_locked(projection).await?;
        let mut selection = self
            .model_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        selection.model = Some(model.to_owned());
        selection.model_profile = model_profile.map(str::to_owned);
        selection.mod_model = Some(model.to_owned());
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
                .map(|attachment| attachment.projection.clone()),
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
        let mut host = match &self.mod_executor {
            Some(executor) => executor.mod_host().await,
            None => None,
        }
        .filter(|host| host.has_event("session.append"));
        let cwd = self.mod_cwd.as_deref().unwrap_or_else(|| {
            self.transcript_path
                .parent()
                .unwrap_or(&self.transcript_path)
        });
        let session = if let Some(current_host) = host.as_ref() {
            match current_host
                .bind_child_turn_step_session(
                    self.mod_session_origin.clone(),
                    self.mod_require_session_origin,
                    cwd,
                )
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(%error, "child session.append binding failed; preserving row");
                    host = None;
                    None
                }
            }
        } else {
            None
        };

        let source_attachment = match source_attachment {
            Some(source_attachment) => {
                if let Some(host) = host.as_ref() {
                    let should_dispatch = self
                        .dispatched_attachment_rows
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(source_attachment.uuid.clone());
                    let rewritten = if should_dispatch {
                        mod_append_source_attachment(
                            host,
                            session.clone(),
                            cwd,
                            self.agent_id,
                            &source_attachment,
                        )
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
                    session,
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
        self.record(&ConversationMessage::System {
            id: lingxi_core::types::MessageId::new(),
            content: serde_json::to_string(&messages)
                .expect("thinking ranges serialize")
                .into(),
            subtype: Some("thinking_stripped".into()),
            api_system: None,
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
        attachment: Utf16JsonProjection,
    ) -> Result<(), lingxi_core::host::FsError> {
        let fs = self.durable_fs()?;
        let _journal = self.journal_lock.lock().await;
        self.recover_attachment_intent().await?;
        let (root, relative, intent) = self.attachment_paths()?;
        let existing = self.read_transcript().await?;
        let entry = self.attachment_entry(message.id(), Some(message), attachment)?;
        let overrides = peer_attachment_overrides(&entry)?;
        let entry_uuid = entry
            .string_at("/uuid")
            .expect("attachment row has its source UUID");
        for line in existing.lines().filter(|line| !line.is_empty()) {
            let decoded = lingxi_core::types::exact_json::parse_exact_json(line)
                .map_err(|_| attachment_io("transcript contains an uncommitted or invalid row"))?;
            let row = Utf16JsonProjection::parse(line)
                .map_err(|_| attachment_io("transcript contains an uncommitted or invalid row"))?;
            if row.string_at("/uuid") == Some(entry_uuid)
                || row.string_at("/message/id") == Some(entry_uuid)
            {
                if row.string_at("/type") != Some("attachment")
                    || !source_projection_matches(&row, &entry, "/agent_id")
                    || !source_projection_matches(&row, &entry, "/message")
                    || !source_projection_matches(&row, &entry, "/attachment")
                    || decoded.utf16_overrides != overrides
                {
                    return Err(attachment_io("peer transcript identity collision"));
                }
                return fs.sync_file_rooted_no_follow(root, relative).await;
            }
        }
        if !existing.is_empty() && !existing.ends_with('\n') {
            return Err(attachment_io("transcript has an unowned partial tail"));
        }
        let mut encoded = entry
            .to_json_string()
            .map_err(|_| attachment_io("typed peer serialization failed"))?
            .into_bytes();
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
        attachment: Utf16JsonProjection,
    ) -> Result<(), lingxi_core::host::FsError> {
        let projection = self.attachment_entry(id, message, attachment)?;
        self.append_projection(projection).await
    }

    fn attachment_entry(
        &self,
        id: lingxi_core::types::MessageId,
        message: Option<&ConversationMessage>,
        attachment: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, lingxi_core::host::FsError> {
        let placeholder = ConversationMessage::user_meta(id, String::new());
        let entry = self.message_entry(message.unwrap_or(&placeholder));
        let mut projection = transcript_entry_projection(&entry, None)
            .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        if message.is_none() {
            projection
                .set_field("message", source_null())
                .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        }
        projection
            .set_field("type", "attachment".into())
            .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        projection
            .set_field("uuid", id.as_uuid().to_string().into())
            .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        projection
            .set_field("attachment", attachment)
            .map_err(|_| attachment_io("transcript exact UTF-16 projection is invalid"))?;
        Ok(projection)
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
            message: ConversationMessage::System {
                id: lingxi_core::types::MessageId::new(),
                content: detail.to_string().into(),
                subtype: Some(format!("agent_{status}")),
                api_system: None,
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
        let serialized = serde_json::to_string(entry).expect("transcript serialization");
        let projection = Utf16JsonProjection::parse(&serialized)
            .map_err(|_| attachment_io("transcript source projection is invalid"))?;
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
        let row_source = intent.line.strip_suffix('\n').unwrap_or(&intent.line);
        let row = Utf16JsonProjection::parse(row_source)
            .map_err(|_| attachment_io("peer append intent is not one JSON row"))?;
        let expected_agent_id = self.agent_id.as_uuid().to_string();
        if !intent.line.ends_with('\n')
            || row.string_at("/type") != Some("attachment")
            || row.string_at("/agent_id") != Some(expected_agent_id.as_str())
            || row.string_at("/attachment/type") != Some("subagent_handback")
        {
            return Err(attachment_io("peer append intent owner is invalid"));
        }
        let envelope_source = row
            .subprojection("/attachment/envelope")
            .map_err(|_| attachment_io("peer append envelope is invalid"))?;
        let envelope_json = envelope_source
            .to_json_string()
            .map_err(|_| attachment_io("peer append envelope is invalid"))?;
        let envelope: lingxi_core::host::handback::HandbackEnvelope =
            serde_json::from_str(&envelope_json)
                .map_err(|_| attachment_io("peer append envelope is invalid"))?;
        let expected_message = envelope.model_message();
        let expected_message_projection = conversation_message_projection(&expected_message)
            .map_err(|_| attachment_io("peer append message is invalid"))?;
        let row_message = row
            .subprojection("/message")
            .map_err(|_| attachment_io("peer append message is invalid"))?;
        let expected_uuid = envelope.receipt.message_id.as_uuid().to_string();
        if !envelope.validate()
            || !matches!(envelope.receipt.recipient, lingxi_core::host::handback::HandbackRecipient::Agent { agent_id, .. } if agent_id == self.agent_id)
            || row.string_at("/uuid") != Some(expected_uuid.as_str())
            || !row_message.semantically_eq(&expected_message_projection)
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
    entry: &Utf16JsonProjection,
) -> Result<lingxi_core::types::exact_json::Utf16Overrides, lingxi_core::host::FsError> {
    let envelope_source = entry
        .subprojection("/attachment/envelope")
        .map_err(|_| attachment_io("peer attachment envelope is invalid"))?;
    let envelope_json = envelope_source
        .to_json_string()
        .map_err(|_| attachment_io("peer attachment envelope is invalid"))?;
    let envelope: lingxi_core::host::handback::HandbackEnvelope =
        serde_json::from_str(&envelope_json)
            .map_err(|_| attachment_io("peer attachment envelope is invalid"))?;
    Ok(envelope.transcript_utf16_overrides())
}

fn source_projection_matches(
    left: &Utf16JsonProjection,
    right: &Utf16JsonProjection,
    pointer: &str,
) -> bool {
    left.subprojection(pointer)
        .ok()
        .zip(right.subprojection(pointer).ok())
        .is_some_and(|(left, right)| left.semantically_eq(&right))
}

fn conversation_message_projection(
    message: &ConversationMessage,
) -> Result<Utf16JsonProjection, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    let mut projection = Utf16JsonProjection::parse(&serde_json::to_string(message)?)?;
    project_message_utf16(&mut projection, message, "");
    Ok(projection)
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
                let ranges = content
                    .string_units("")
                    .and_then(|units| Utf16JsonProjection::parse_utf16_json(&units).ok())
                    .and_then(|source| source.to_json_string().ok())
                    .and_then(|json| serde_json::from_str(&json).ok());
                if let Some(ranges) = ranges {
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
    use crate::mod_turn_step::tests::{Origin, Session};
    use lingxi_core::host::subagent_spawn::SessionOriginCapability;
    use lingxi_core::types::SessionId;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn child_session_append_keeps_launch_origin_after_switch() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("append-origin.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
          on('session.append', async ($, event, next) => {
            try { await $.session.model(); } catch (_) {}
            return next(event);
          });
        }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "append-origin",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
        )
        .await
        .unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let session: Arc<dyn hooks::mods::ModSessionContext> = Arc::new(Session {
            root: dir.path().to_path_buf(),
            origin: None,
            model_reads: reads.clone(),
        });
        host.attach_background_context(Arc::downgrade(&session));
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let executor = Arc::new(hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(test_harness::mocks::MockHttpTransport::new()),
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        ));
        let original = SessionId::new();
        let other = SessionId::new();
        let active = Arc::new(Mutex::new((original, 1)));
        let origin: Arc<dyn SessionOriginCapability> = Arc::new(Origin {
            id: original,
            generation: 1,
            active: active.clone(),
            root: dir.path().to_path_buf(),
        });
        for (current, generation, capability) in [
            (original, 1, Some(origin.clone())),
            (other, 2, Some(origin.clone())),
            (original, 3, Some(origin.clone())),
            (original, 3, None),
        ] {
            *active.lock().unwrap() = (current, generation);
            let writer = AgentTranscriptWriter::without_persistence(
                dir.path().join("child.jsonl"),
                AgentId::new(),
            )
            .with_mod_append(Some(executor.clone()), dir.path().to_path_buf(), None)
            .with_mod_session_origin(capability, true);
            let mut message = ConversationMessage::user(MessageId::new(), "hello".into());
            writer.accept_retained(&mut message, &[]).await.unwrap();
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1, "only live A may read");
    }

    #[test]
    fn session_append_rewrite_preserves_refused_message_id_utf16_carrier() {
        let refused_message_id = Utf16JsonProjection::parse(r#""refused-\ud800-id""#).unwrap();
        let mut original = ConversationMessage::user(MessageId::new(), "original".into());
        if let ConversationMessage::User {
            refused_message_id: carrier,
            ..
        } = &mut original
        {
            *carrier = Some(refused_message_id.clone());
        }

        let incoming = Utf16JsonProjection::parse(r#"{"content":[]}"#).unwrap();
        let rewritten = rewrite_append_message(&original, &incoming).unwrap();
        let ConversationMessage::User {
            refused_message_id: Some(rewritten),
            ..
        } = rewritten
        else {
            panic!("session.append must retain refusal metadata");
        };
        assert_eq!(
            rewritten.string_units(""),
            refused_message_id.string_units("")
        );
    }

    #[test]
    fn append_overlay_replays_after_every_duplicate_source_tool_identity() {
        let first = ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::from("source-a"),
            name: "Read".into(),
            input: source_from_value(&serde_json::json!({"file_path":"a"})),
            provider_id: Some("toolu_duplicate".into()),
        };
        let second = ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::from("source-b"),
            name: "Read".into(),
            input: source_from_value(&serde_json::json!({"file_path":"b"})),
            provider_id: Some("toolu_duplicate".into()),
        };
        let accepted_text = ContentBlock::Text {
            text: "accepted suffix".into(),
            citations: Some(None),
        };
        let rewritten = rewrite_append_blocks(
            &[first.clone(), second.clone()],
            &[
                source_from_value(&serde_json::json!({
                    "type":"tool_use",
                    "id":"toolu_duplicate",
                    "name":"Write",
                    "input":{"replacement":true},
                })),
                source_from_value(&serde_json::json!({"type":"text","text":"accepted suffix"})),
            ],
        );
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
        assert!(projected_text.get_property("citations").is_some());
        assert!(projected_text
            .get_property("citations")
            .is_some_and(|value| value.is_null()));

        let omitted = ContentBlock::ToolResult {
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_1"),
            content: lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                ("ok").to_owned(),
                ("ok").encode_utf16().collect(),
            )
            .expect("test text is valid UTF-16"),
            is_error: None,
            provider_tool_use_id: None,
            content_blocks: None,
        };
        assert!(append_content_block(&omitted)
            .get_property("is_error")
            .is_none());

        let explicit_false = ContentBlock::ToolResult {
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_1"),
            content: lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                ("ok").to_owned(),
                ("ok").encode_utf16().collect(),
            )
            .expect("test text is valid UTF-16"),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        };
        assert_eq!(
            append_content_block(&explicit_false)
                .get_property("is_error")
                .and_then(|value| value.as_bool()),
            Some(false)
        );

        let accepted_text = rewrite_append_blocks(
            &[],
            &[source_from_value(&serde_json::json!({
                "type":"text",
                "text":"new answer",
                "citations":null,
            }))],
        );
        assert!(matches!(
            accepted_text.as_slice(),
            [ContentBlock::Text {
                citations: Some(None),
                ..
            }]
        ));
        let projected_accepted_text = append_content_block(&accepted_text[0]);
        assert!(projected_accepted_text.get_property("citations").is_some());
        assert!(projected_accepted_text
            .get_property("citations")
            .is_some_and(|value| value.is_null()));

        let old_error = ContentBlock::ToolResult {
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_1"),
            content: lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                ("failed").to_owned(),
                ("failed").encode_utf16().collect(),
            )
            .expect("test text is valid UTF-16"),
            is_error: Some(true),
            provider_tool_use_id: None,
            content_blocks: None,
        };
        let omitted_error = rewrite_append_tool_result(
            &old_error,
            &source_from_value(&serde_json::json!({
                "type":"tool_result",
                "content":"accepted",
            })),
        );
        assert!(matches!(
            omitted_error,
            ContentBlock::ToolResult { is_error: None, .. }
        ));
    }

    #[test]
    fn unchanged_opaque_anthropic_text_echo_preserves_source_metadata() {
        let source = ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: source_from_value(&serde_json::json!({
                "type":"text",
                "text":"provider answer",
                "citations":[{"type":"future_citation"}],
                "extra_native":{"kept":289},
            })),
        };
        let projected = append_content_block(&source);
        assert_eq!(
            rewrite_append_blocks(
                std::slice::from_ref(&source),
                std::slice::from_ref(&projected),
            ),
            vec![source.clone()]
        );

        let mut changed = projected;
        changed
            .set_field("text", "replacement answer".into())
            .unwrap();
        assert_eq!(
            rewrite_append_blocks(&[source], &[changed]),
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
        assert!(writer
            .record_model_selection("shared-model", Some("b"))
            .await
            .is_err());
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
        let message = ConversationMessage::User {
            id: MessageId::new(),
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
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
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        };
        let mut attachment_units = "context ".encode_utf16().collect::<Vec<_>>();
        attachment_units.push(0xdfff);
        let mut attachment = source_from_value(&serde_json::json!({
            "type":"hook_additional_context",
            "content":[String::from_utf16_lossy(&attachment_units), "valid Unicode Ω 🧪"],
        }));
        attachment
            .set_string_units("/content/0", &attachment_units)
            .unwrap();
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
        assert_eq!(row.string_at("/type"), Some("attachment"));
        assert_eq!(row.string_at("/message/content/0/text"), Some("reminder �"));
        assert_eq!(
            row.string_units("/message/content/0/text"),
            Some(message_units)
        );
        assert_eq!(
            row.string_at("/message/content/1/text"),
            Some("valid Unicode λ 😀")
        );
        assert_eq!(row.string_at("/attachment/content/0"), Some("context �"));
        assert_eq!(
            row.string_units("/attachment/content/0"),
            Some(attachment_units)
        );
        assert_eq!(
            row.string_at("/attachment/content/1"),
            Some("valid Unicode Ω 🧪")
        );
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
        let source_projection = source_from_value(&source);
        writer.register_source_attachment(&message, source_projection.clone());
        writer.record(&message).await.unwrap();
        writer.record(&message).await.unwrap();
        let entries = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<TranscriptEntry>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries[0].source_attachment, Some(source_projection));
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
            let removed = ConversationMessage::Assistant {
                id: removed_id,
                content: vec![ContentBlock::Text {
                    text: "discarded server row".into(),
                    citations: None,
                }],
                stop_reason: None,
                per_turn_effort: None,

                resumable: None,
            };
            let retained = ConversationMessage::user_meta(MessageId::new(), "keep me".into());
            writer.record(&removed).await.unwrap();
            writer.record(&retained).await.unwrap();
            writer
                .record_context_attachment(
                    removed_id,
                    Some(&removed),
                    Utf16JsonProjection::from_serde_value(
                        &serde_json::json!({"type":"date","date":"2026-10-04"}),
                    )
                    .unwrap(),
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
        let refusal_text = Utf16JsonProjection::from("declined target");
        let mut row = lingxi_core::host::ServerFallbackApiErrorRow::new(
            &refusal_text,
            "2026-10-04T12:00:00.000Z".into(),
        );
        let stop_details = Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"type":"refusal","category":"safety"}),
        )
        .unwrap();
        row.set_refusal(Some("request-17".into()), stop_details);
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
        host.load(
            "append-retry",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
        )
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
            source_from_value(&serde_json::json!({
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
            entry.source_attachment.unwrap().string_at("/content/0"),
            Some("source edited")
        );
        let calls = host
            .dispatch(
                "prompt.submit",
                Utf16JsonProjection::from_serde_value(&serde_json::json!({"text":"probe"}))
                    .unwrap(),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        assert_eq!(
            calls.get_property("text").and_then(|value| value.as_str()),
            Some("2"),
            "attachment and row dispatch once each"
        );
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
        host.load(
            "append-utf16",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
        )
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
            source_from_value(&serde_json::json!({
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
        assert_eq!(
            row.string_at("/source_attachment/content/0"),
            Some("accepted�")
        );
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
        host.load(
            "append-nontext",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
        )
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
            source_from_value(&serde_json::json!({
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
            row.string_at("/source_attachment/content/0"),
            Some("original"),
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
        let assistant = |id, text: &str| ConversationMessage::Assistant {
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
            per_turn_effort: None,

            resumable: None,
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
