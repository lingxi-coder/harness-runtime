//! Conversions between [`protocol`] types and [`llm_runtime`] types.
//!
//! The two crates use parallel but structurally equivalent type hierarchies.
//! This module is the single place that translates between them so the rest of
//! the agent crate can stay unaware of `llm_runtime` internals.
//!
//! # Mapping table
//!
//! | Protocol | `llm_runtime` |
//! |---|---|
//! | `ConversationMessage::User` | `Message { role: "user", .. }` |
//! | `ConversationMessage::Assistant` | `Message { role: "assistant", .. }` |
//! | `ConversationMessage::System` | rejected (`LlmError::InvalidRequest`) |
//! | `ContentBlock::Text` | `ContentBlock::Text { cache_control: None }` |
//! | `ContentBlock::ToolUse { id, name, input }` | `ContentBlock::ToolCall { id: id.to_string(), name, input }` |
//! | `ContentBlock::ToolResult { tool_use_id, content, is_error }` | `ContentBlock::ToolResult { tool_call_id: …, output: JsonPayload, is_error, cache_control: None }` |
//! | `ContentBlock::Thinking { thinking, signature }` | `ContentBlock::Reasoning { text: thinking, signature }` |
//! | `ContentBlock::Image { source: ImageSource::Base64 { media_type, data } }` | `ContentBlock::Image { media_type, bytes: base64_decode(data) }` |
//! | `ContentBlock::Image { source: ImageSource::Url { url } }` | `ContentBlock::ImageUrl { url }` |
//! | `ContentBlock::Document { source: DocumentSource::Base64 { media_type, data } }` | `ContentBlock::Document { media_type, bytes: base64_decode(data) }` |
//! | `ContentBlock::MediaAnalysis { analysis }` | `ContentBlock::Text { text: render_media_analysis(analysis) }` |
//!
//! Low-frequency server-side variants ARE round-tripped (ingest preserves them
//! into protocol blocks, and egress here replays them verbatim back to
//! `llm_runtime`): `RedactedThinking`, `ServerToolUse`, `ConnectorText`,
//! `AdvisorToolResult` — so resume/replay bytes stay intact when those betas are
//! active. (`ImageUrl` is decode-only on the inbound side.)

use crate::{ContentBlock as LlmBlock, LlmError, Message, ToolDeclaration};
use base64::Engine as _;
use lingxi_core::host::SourceToolWireDefinition;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use lingxi_core::types::{
    ContentBlock as ProtoBlock, ConversationMessage, DocumentSource, ImageSource, MediaAnalysis,
    MessageId, ToolUseId,
};
use lingxi_llm_client::protocol::JsonPayload;
use std::collections::HashSet;

/// One request-local Native token reminder that belongs at the end of an
/// Anthropic tool result. Its message identity lets the API-bound clone remove
/// the separate reminder only when the intended tool result was updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicToolResultReminderSuffix {
    /// Exact tool-use identity of the result to decorate.
    pub tool_use_id: ToolUseId,
    /// Identity of the standalone reminder message in the request clone.
    pub reminder_message_id: MessageId,
    /// Exact reminder text as a root JSON string.
    pub text: Utf16JsonProjection,
}

/// Append a request-local reminder to the identified tool result and remove
/// its standalone message from this API-bound clone. Durable history remains
/// untouched. Returns `false` without changing either message when the target
/// or exact reminder source is absent, or when the result shape is unsupported.
pub fn append_anthropic_tool_result_reminder(
    messages: &mut Vec<ConversationMessage>,
    suffix: &AnthropicToolResultReminderSuffix,
) -> bool {
    let Some(reminder_units) = suffix.text.string_units("") else {
        return false;
    };
    let Some(reminder_index) = messages.iter().position(|message| {
        message.id() == suffix.reminder_message_id
            && single_text_message_units(message).as_deref() == Some(reminder_units.as_slice())
    }) else {
        return false;
    };
    let Some((message_index, block_index)) = messages
        .iter()
        .enumerate()
        .find_map(|(message_index, message)| {
            let ConversationMessage::User { content, .. } = message else {
                return None;
            };
            content
                .iter()
                .position(|block| {
                    matches!(block, ProtoBlock::ToolResult { tool_use_id, .. } if tool_use_id == &suffix.tool_use_id)
                })
                .map(|block_index| (message_index, block_index))
        })
    else {
        return false;
    };

    let ConversationMessage::User { content, .. } = &mut messages[message_index] else {
        return false;
    };
    let ProtoBlock::ToolResult {
        content: result_text,
        content_blocks,
        ..
    } = &mut content[block_index]
    else {
        unreachable!("the selected block is a tool result");
    };

    let appended = if let Some(blocks) = content_blocks {
        append_text_block(blocks, &suffix.text)
    } else {
        append_to_root_string(result_text, &reminder_units)
    };
    if !appended {
        return false;
    }

    messages.remove(reminder_index);
    true
}

fn single_text_message_units(message: &ConversationMessage) -> Option<Vec<u16>> {
    let ConversationMessage::User {
        content, is_meta, ..
    } = message
    else {
        return None;
    };
    if !*is_meta || content.len() != 1 {
        return None;
    }
    match content.first()? {
        ProtoBlock::Text { text, .. } => Some(text.encode_utf16().collect()),
        ProtoBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        _ => None,
    }
}

fn append_to_root_string(target: &mut Utf16JsonProjection, suffix: &[u16]) -> bool {
    let Some(mut units) = target.string_units("") else {
        return false;
    };
    units.extend([b'\n' as u16, b'\n' as u16]);
    units.extend_from_slice(suffix);
    let display = String::from_utf16_lossy(&units);
    let Ok(updated) = Utf16JsonProjection::root_string(display, units) else {
        return false;
    };
    *target = updated;
    true
}

fn append_text_block(target: &mut Utf16JsonProjection, text: &Utf16JsonProjection) -> bool {
    let Some(length) = target.array_len("") else {
        return false;
    };
    let mut blocks = Vec::with_capacity(length.saturating_add(1));
    for index in 0..length {
        let pointer = format!("/{index}");
        let Ok(mut block) = target.subprojection(&pointer) else {
            return false;
        };
        if index + 1 == length && target.string_at(&format!("{pointer}/type")) == Some("text") {
            if let Some(mut units) = target
                .subprojection(&format!("{pointer}/text"))
                .ok()
                .and_then(|text| text.string_units(""))
            {
                units.push(b'\n' as u16);
                let display = String::from_utf16_lossy(&units);
                let Ok(text) = Utf16JsonProjection::root_string(display, units) else {
                    return false;
                };
                if block.set_field("text", text).is_err() {
                    return false;
                }
            }
        }
        blocks.push(block);
    }
    let Ok(reminder_block) = Utf16JsonProjection::object(vec![
        ("type".to_owned(), Utf16JsonProjection::from("text")),
        ("text".to_owned(), text.clone()),
    ]) else {
        return false;
    };
    blocks.push(reminder_block);
    let Ok(updated) = Utf16JsonProjection::array(blocks) else {
        return false;
    };
    *target = updated;
    true
}

/// API-bound conversation blocks retain the identities of the Host messages
/// that contributed them. The sidecar is removed before provider conversion.
#[derive(Debug, Clone)]
pub(crate) struct ConversationMessagesWithSources {
    pub(crate) messages: Vec<ConversationMessage>,
    pub(crate) block_sources: Vec<Vec<HashSet<lingxi_core::types::MessageId>>>,
}

impl ConversationMessagesWithSources {
    pub(crate) fn new(messages: Vec<ConversationMessage>) -> Self {
        let block_sources = messages
            .iter()
            .map(|message| {
                let (id, len) = match message {
                    ConversationMessage::User { id, content, .. }
                    | ConversationMessage::Assistant { id, content, .. } => {
                        (Some(*id), content.len())
                    }
                    ConversationMessage::System {
                        id,
                        api_system: Some(message),
                        ..
                    } => (Some(*id), message.content.len()),
                    ConversationMessage::System { .. } => (None, 0),
                };
                (0..len)
                    .map(|_| id.into_iter().collect::<HashSet<_>>())
                    .collect()
            })
            .collect();
        Self {
            messages,
            block_sources,
        }
    }

    pub(crate) fn contributing_message_ids(&self) -> HashSet<lingxi_core::types::MessageId> {
        self.block_sources
            .iter()
            .flatten()
            .flat_map(|sources| sources.iter().copied())
            .collect()
    }
}

/// Convert a `Vec<ConversationMessage>` into `Vec<llm_runtime::Message>`.
///
/// API-system `System` rows are retained; transcript-only `System` rows are
/// rejected because they do not belong in provider message input.
///
/// Returns `Err(LlmError::InvalidRequest)` if any content block cannot be
/// converted (e.g. bad base64).
pub fn to_llm_messages(messages: Vec<ConversationMessage>) -> Result<Vec<Message>, LlmError> {
    messages.into_iter().map(convert_message).collect()
}

/// Merge consecutive `User` messages into a single user turn (claude-code
/// `normalizeMessagesForAPI` consecutive-user merge + `mergeUserMessages`,
/// `utils/messages.ts:2411`).
///
/// `Assistant`/`System` messages pass through unchanged and act as separators.
/// The merged message keeps the FIRST message's id. The two operands' content
/// blocks are merged via the faithful claude-code merge pipeline
/// `hoistToolResults(joinTextAtSeam(a, b))`:
///
/// * [`join_text_at_seam`] — when `a`'s last block and `b`'s first block are
///   both `Text`, append `'\n'` to `a`'s last text before concatenating, so two
///   queued text prompts `"2 + 2"` + `"3 + 3"` don't reach the model glued as
///   `"2 + 23 + 3"` (the API concatenates adjacent text blocks with no
///   separator). The `\n` goes on `a`'s side so no block's `startsWith`
///   classification changes (`joinTextAtSeam`, `messages.ts:2505`).
/// * [`hoist_tool_results`] — stable-partition `ToolResult` blocks to the front
///   so they lead the merged user turn, avoiding "tool result must follow tool
///   use" API errors (`hoistToolResults`, `messages.ts:2470`).
///
/// Single or non-adjacent user messages are unaffected (identity).
///
/// claude-code rationale: "Bedrock doesn't support multiple user messages in a
/// row; 1P API merges them into a single user turn."
///
/// # Remainder of `normalizeMessagesForAPI` that is N/A to LingXi
///
/// claude-code's full `normalizeMessagesForAPI` (`messages.ts:1989-2370`) runs
/// several other transforms ahead of the merge. They have no substrate in
/// LingXi's message model and are therefore deliberately NOT ported (porting a
/// stub would be an unfaithful divergence):
///
/// * `reorderAttachmentsForAPI` / `isVirtual` filtering — N/A: there is no
///   attachment-typed or virtual `ConversationMessage`; the protocol has only
///   `User`/`Assistant`/`System` (`protocol/src/messages.rs:168`). Attachments
///   are already plain `User` content blocks inserted in position by the caller.
/// * `progress` / synthetic-api-error filtering — N/A: no `progress` or
///   `synthetic_api_error` message types exist. `System` messages (the
///   transcript-only `Conversation compacted` boundary marker) ARE filtered
///   here — dropped before the wire, claude-code `isVisibleInTranscriptOnly` —
///   since `convert_message` rejects any `System` left in the messages vec.
/// * `stripTargets` error-block stripping (PDF/image/request-too-large → strip
///   `document`/`image` from the preceding `isMeta` user) — N/A: requires an
///   `isMeta` flag and `isSyntheticApiErrorMessage` markers; the protocol has
///   no `isMeta` flag (see `conversation.rs:1389`, `turn_loop.rs:940`) and no
///   `RequestTooLarge`/`PdfTooLarge` markers.
/// * assistant tool-input normalization (`normalizeToolInputForAPI` stripping
///   `plan`/`caller`/synthetic-edit fields) — N/A: LingXi has no
///   `normalizeToolInput` *producer* to reverse; tool inputs are model-authored
///   and pass through unmodified (`tools/plan/src/plan_mode.rs:150,466`).
///   Stripping a model-authored field would corrupt faithful round-trips.
#[must_use]
pub fn normalize_messages_for_api(messages: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
    normalize_messages_for_api_with_tool_search(messages, true, None)
}

/// Request-aware normalization for dynamic tool loading.
///
/// When tool search is unavailable (for example after switching Sonnet →
/// Haiku), stale `tool_reference` blocks are removed from the API-bound clone.
/// When it remains enabled, references to tools no longer present in the live
/// registry are removed. A surviving reference receives Claude Code's
/// transient `Tool loaded.` sibling boundary; none of these mutations touch the
/// persisted transcript.
#[must_use]
pub fn normalize_messages_for_api_with_tool_search(
    messages: Vec<ConversationMessage>,
    tool_search_enabled: bool,
    available_tool_names: Option<&std::collections::HashSet<String>>,
) -> Vec<ConversationMessage> {
    normalize_messages_for_api_with_tool_search_and_sources(
        ConversationMessagesWithSources::new(messages),
        &HashSet::new(),
        &std::collections::HashMap::new(),
        tool_search_enabled,
        available_tool_names,
    )
    .messages
}

pub(crate) fn normalize_messages_for_api_with_tool_search_and_sources(
    messages: ConversationMessagesWithSources,
    typed_attachment_ids: &HashSet<lingxi_core::types::MessageId>,
    task_deliveries: &std::collections::HashMap<
        lingxi_core::types::MessageId,
        lingxi_core::host::task_delivery::TaskDeliveryRequest,
    >,
    tool_search_enabled: bool,
    available_tool_names: Option<&std::collections::HashSet<String>>,
) -> ConversationMessagesWithSources {
    // Claude's query path first projects from the most recent compact boundary
    // (including the marker), then drops the transcript-only marker below.
    // Local compaction already replaces the active history, but SDK history
    // replay can deliver pre-boundary messages followed by the boundary, so the
    // slice is required here as a final model-facing invariant.
    let boundary_index = messages.messages.iter().rposition(|message| {
        matches!(
            message,
            ConversationMessage::System {
                subtype: Some(subtype),
                ..
            } if subtype == "compact_boundary"
        )
    });
    let start = boundary_index.unwrap_or(0);
    let mut out = ConversationMessagesWithSources {
        messages: Vec::with_capacity(messages.messages.len() - start),
        block_sources: Vec::with_capacity(messages.messages.len() - start),
    };
    // Native Mo consumes a delivery UUID at most once within this request.
    // The durable source map remains untouched and is reconsidered next time.
    let mut consumed_deliveries = HashSet::new();
    for (mut msg, mut block_sources) in messages
        .messages
        .into_iter()
        .zip(messages.block_sources)
        .skip(start)
    {
        let carrier_id = msg.id();
        if consumed_deliveries.contains(&carrier_id) {
            continue;
        }
        if let Some(plan) = task_deliveries.get(&carrier_id).and_then(|delivery| {
            crate::task_delivery::plan_task_delivery(&out, carrier_id, delivery)
                .ok()
                .flatten()
        }) {
            // This converter materializes attachment/API-system rows
            // as it iterates, so the prefix has no deferred tail to
            // flush. Apply at this exact position after those rows.
            let tail_flushed = plan.requires_tail_flush();
            if plan.apply(&mut out, tail_flushed).is_ok() {
                consumed_deliveries.insert(carrier_id);
                continue;
            }
        }
        let is_typed_attachment = match &msg {
            ConversationMessage::User { id, .. } => typed_attachment_ids.contains(id),
            _ => false,
        };
        if let ConversationMessage::User {
            id,
            api_message_override: Some(message),
            ..
        } = &msg
        {
            let id = *id;
            let message = message.clone();
            block_sources = (0..message.content.len())
                .map(|_| [id].into_iter().collect())
                .collect();
            msg = ConversationMessage::api_system(id, message);
        }
        // `stripAdvisorBlocks` (claude-code `claude.ts:1305`): drop
        // `advisor_tool_result` (no advisor beta) and `connector_text` (no
        // encode path — the Anthropic encoder rejects them) before the wire.
        // The blocks remain PRESERVED in the JSONL transcript (history); only
        // the outgoing clone is stripped. `redacted_thinking`/`server_tool_use`
        // are kept (they round-trip).
        match &mut msg {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => {
                let mut retained_content = Vec::with_capacity(content.len());
                let mut retained_sources = Vec::with_capacity(block_sources.len());
                for (block, sources) in content.drain(..).zip(block_sources.drain(..)) {
                    if !matches!(
                        &block,
                        ProtoBlock::ConnectorText { .. } | ProtoBlock::AdvisorToolResult { .. }
                    ) {
                        retained_content.push(block);
                        retained_sources.push(sources);
                    }
                }
                *content = retained_content;
                block_sources = retained_sources;
            }
            // Transcript-only markers (the `Conversation compacted` boundary,
            // `compaction/src/boundary.rs`) stay in `session.history` for JSONL
            // + TUI but MUST NOT reach the wire — the real system prompt rides
            // the `system` parameter and `convert_message` rejects any `System`
            // here. claude-code `isVisibleInTranscriptOnly`. Dropping pre-merge
            // also lets the surrounding same-role messages collapse below.
            ConversationMessage::System {
                api_system: Some(_),
                ..
            } => {}
            ConversationMessage::System { .. } => continue,
        }
        if let ConversationMessage::User { content, .. } = &mut msg {
            let previous_len = content.len();
            normalize_tool_references(content, tool_search_enabled, available_tool_names);
            if content.len() > previous_len {
                let sources = block_sources
                    .iter()
                    .flat_map(|ids| ids.iter().copied())
                    .collect();
                block_sources.resize_with(content.len(), HashSet::new);
                block_sources[previous_len] = sources;
            }
        }
        match (
            out.messages.last_mut(),
            out.block_sources.last_mut(),
            msg,
            block_sources,
        ) {
            (
                Some(ConversationMessage::User {
                    content: prev_content,
                    ..
                }),
                Some(prev_sources),
                ConversationMessage::User {
                    content: new_content,
                    ..
                },
                new_sources,
            ) => {
                if is_typed_attachment {
                    join_typed_attachment_content(
                        prev_content,
                        prev_sources,
                        new_content,
                        new_sources,
                    );
                } else {
                    join_text_at_seam(prev_content, prev_sources, new_content, new_sources);
                }
                hoist_tool_results(prev_content, prev_sources);
            }
            (
                Some(ConversationMessage::System {
                    api_system: Some(previous),
                    ..
                }),
                Some(previous_sources),
                ConversationMessage::System {
                    api_system: Some(next),
                    ..
                },
                next_sources,
            ) if previous.output_config.is_none() && next.output_config.is_none() => {
                join_api_system_content(
                    &mut previous.content,
                    previous_sources,
                    next.content,
                    next_sources,
                );
            }
            // `mergeAssistantMessages` (claude-code `messages.ts`): the
            // per-content-block assistant lines emitted by the streaming writer
            // (one JSONL line per `content_block_stop`) re-collapse to a single
            // assistant turn on the wire. claude-code keys on the shared API
            // `message.id`; here we key on adjacency, which is equivalent —
            // within a valid transcript consecutive `Assistant` messages with no
            // intervening `User`/tool_result always belong to the same response.
            // No `hoist_tool_results` (assistants carry `tool_use`, not
            // `tool_result`, and block order must be preserved). A freshly-built
            // single merged assistant is unaffected (identity); a resumed history
            // of split lines collapses back to one turn.
            (
                Some(ConversationMessage::Assistant {
                    content: prev_content,
                    ..
                }),
                Some(prev_sources),
                ConversationMessage::Assistant {
                    content: new_content,
                    ..
                },
                new_sources,
            ) => {
                join_text_at_seam(prev_content, prev_sources, new_content, new_sources);
            }
            (_, _, msg, block_sources) => {
                out.messages.push(msg);
                out.block_sources.push(block_sources);
            }
        }
    }
    out
}

fn normalize_tool_references(
    content: &mut Vec<ProtoBlock>,
    tool_search_enabled: bool,
    available_tool_names: Option<&std::collections::HashSet<String>>,
) {
    const TURN_BOUNDARY: &str = "Tool loaded.";
    const DISABLED_PLACEHOLDER: &str = "[Tool references removed - tool search not enabled]";
    const UNAVAILABLE_PLACEHOLDER: &str = "[Tool references removed - tools no longer available]";

    let mut has_surviving_reference = false;
    for block in content.iter_mut() {
        let ProtoBlock::ToolResult {
            content_blocks: Some(blocks),
            ..
        } = block
        else {
            continue;
        };
        let count = blocks
            .array_len("")
            .expect("tool result content blocks are stored as a JSON array");
        let mut had_reference = false;
        let mut has_reference = false;
        let mut retained = Vec::with_capacity(count);
        for index in 0..count {
            let pointer = format!("/{index}");
            let candidate = blocks
                .at_pointer(&pointer)
                .expect("array index is within the content-block array");
            if !is_tool_reference(candidate) {
                retained.push(
                    blocks
                        .subprojection(&pointer)
                        .expect("content block is a valid JSON subtree"),
                );
                continue;
            }

            had_reference = true;
            if !tool_search_enabled {
                continue;
            }
            // Oracle xPy: `let a=s.tool_name; if(!a)return!0; return t.has(W4(a))`.
            // A reference with a falsy tool_name (missing, non-string, or empty)
            // is KEPT; otherwise it survives iff its normalized name is available.
            let name = candidate
                .get_property("tool_name")
                .and_then(JsonValueView::as_str)
                .filter(|name| !name.is_empty());
            let keep = name.is_none_or(|name| {
                available_tool_names
                    .is_none_or(|available| available.contains(normalize_tool_name(name)))
            });
            if keep {
                has_reference = true;
                retained.push(
                    blocks
                        .subprojection(&pointer)
                        .expect("content block is a valid JSON subtree"),
                );
            }
        }
        if !had_reference {
            continue;
        }
        has_surviving_reference |= has_reference;
        if retained.is_empty() {
            retained.push(
                Utf16JsonProjection::object(vec![
                    ("type".to_owned(), Utf16JsonProjection::from("text")),
                    (
                        "text".to_owned(),
                        Utf16JsonProjection::from(if tool_search_enabled {
                            UNAVAILABLE_PLACEHOLDER
                        } else {
                            DISABLED_PLACEHOLDER
                        }),
                    ),
                ])
                .expect("generated tool-reference placeholder is a valid JSON object"),
            );
        }
        *blocks = Utf16JsonProjection::array(retained)
            .expect("retained tool-result content blocks form a valid JSON array");
    }

    if tool_search_enabled
        && has_surviving_reference
        && !content.iter().any(
            |block| matches!(block, ProtoBlock::Text { text, .. } if text.starts_with(TURN_BOUNDARY)),
        )
    {
        content.push(ProtoBlock::Text {
            text: TURN_BOUNDARY.to_string(),
            citations: None,
        });
    }
}

/// Claude's persisted-tool alias normalization used when validating historical
/// tool_reference blocks against the current catalog.
/// Current alias map excludes the removed Agent tool alias. A
/// tool_reference whose (normalized) name is absent from the available set is
/// dropped, so every historical alias must be present or valid references get
/// silently stripped from the API-bound message.
fn normalize_tool_name(name: &str) -> &str {
    match name {
        "KillShell" | "KillBash" => "TaskStop",
        "ListPeers" => "ListAgents",
        "Brief" => "SendUserMessage",
        "ListMcpResources" => "ListMcpResourcesTool",
        "ReadMcpResource" => "ReadMcpResourceTool",
        "ReadMcpResourceDir" => "ReadMcpResourceDirTool",
        current => current,
    }
}

fn is_tool_reference(value: JsonValueView<'_>) -> bool {
    value.get_property("type").and_then(JsonValueView::as_str) == Some("tool_reference")
}

/// Append `b` onto `a`, first joining a text|text seam with a `'\n'`.
///
/// Faithful port of claude-code `joinTextAtSeam` (`utils/messages.ts:2505`):
/// when `a`'s last block and `b`'s first block are both JavaScript text, the
/// `'\n'` is appended to `a`'s last text so no block's leading code units
/// change. `TextJsUtf16` is the source-preserving representation of that same
/// JavaScript string and follows the identical seam rule.
fn join_text_at_seam(
    a: &mut Vec<ProtoBlock>,
    a_sources: &mut Vec<HashSet<lingxi_core::types::MessageId>>,
    mut b: Vec<ProtoBlock>,
    mut b_sources: Vec<HashSet<lingxi_core::types::MessageId>>,
) {
    if b.first().is_some_and(is_text_block) {
        if let Some(last) = a.last_mut().filter(|block| is_text_block(block)) {
            append_newline_to_text_block(last);
        }
    }
    a.append(&mut b);
    a_sources.append(&mut b_sources);
}

fn is_text_block(block: &ProtoBlock) -> bool {
    matches!(
        block,
        ProtoBlock::Text { .. } | ProtoBlock::TextJsUtf16 { .. }
    )
}

fn append_newline_to_text_block(block: &mut ProtoBlock) {
    match block {
        ProtoBlock::Text { text, .. } => text.push('\n'),
        ProtoBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            ..
        } => {
            text.push('\n');
            utf16_code_units.push(b'\n' as u16);
        }
        _ => unreachable!("only JavaScript text blocks receive a seam newline"),
    }
}

/// Append a Host row identified as a typed transcript attachment. Native's
/// `lPn`/`tMn` attachment path concatenates ordinary content blocks without
/// `zO`'s user-message newline seam. When the prior row ends in a tool result,
/// `tMn` folds text into the result using its special string/array rules.
fn join_typed_attachment_content(
    previous: &mut Vec<ProtoBlock>,
    previous_sources: &mut Vec<HashSet<lingxi_core::types::MessageId>>,
    mut incoming: Vec<ProtoBlock>,
    mut incoming_sources: Vec<HashSet<lingxi_core::types::MessageId>>,
) {
    let incoming_ids = incoming_sources
        .iter()
        .flat_map(|sources| sources.iter().copied())
        .collect::<HashSet<_>>();
    let incoming_text = incoming
        .iter()
        .map(text_block_projection)
        .collect::<Option<Vec<_>>>();

    let Some(ProtoBlock::ToolResult {
        content: result_text,
        content_blocks,
        provider_tool_use_id,
        ..
    }) = previous.last_mut()
    else {
        previous.append(&mut incoming);
        previous_sources.append(&mut incoming_sources);
        return;
    };

    // Native keeps task polling and get-task result content out of the generic
    // reminder-fold path. LingXi has the exact provider tool-use identity for
    // these protocol-owned result families.
    if provider_tool_use_id
        .as_deref()
        .is_some_and(|id| id.starts_with("gettask_") || id.starts_with("poll_"))
        || incoming_text
            .as_ref()
            .is_none_or(|blocks| blocks.iter().any(is_protected_tool_result_text))
    {
        previous.append(&mut incoming);
        previous_sources.append(&mut incoming_sources);
        return;
    }

    let Some(incoming_text) = incoming_text else {
        previous.append(&mut incoming);
        previous_sources.append(&mut incoming_sources);
        return;
    };

    let merged = if let Some(blocks) = content_blocks {
        append_native_tool_result_text_blocks(blocks, &incoming_text)
    } else {
        append_native_tool_result_text(result_text, &incoming_text)
    };
    if merged {
        if let Some(sources) = previous_sources.last_mut() {
            sources.extend(incoming_ids);
        }
    } else {
        previous.append(&mut incoming);
        previous_sources.append(&mut incoming_sources);
    }
}

fn text_block_projection(block: &ProtoBlock) -> Option<Utf16JsonProjection> {
    let (units, citations) = match block {
        ProtoBlock::Text { text, citations } => (text.encode_utf16().collect(), citations),
        ProtoBlock::TextJsUtf16 {
            utf16_code_units,
            citations,
            ..
        } => (utf16_code_units.clone(), citations),
        _ => return None,
    };
    let text = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).ok()?;
    let mut fields = vec![
        ("type".to_owned(), Utf16JsonProjection::from("text")),
        ("text".to_owned(), text),
    ];
    if let Some(citations) = citations {
        fields.push((
            "citations".to_owned(),
            citations
                .clone()
                .unwrap_or_else(|| Utf16JsonProjection::parse("null").expect("null is valid")),
        ));
    }
    Utf16JsonProjection::object(fields).ok()
}

fn is_protected_tool_result_text(block: &Utf16JsonProjection) -> bool {
    block
        .string_at("/text")
        .is_some_and(|text| text.starts_with("<system-reminder>\n"))
}

/// Native `DQe` path for tool results with string content: trim the result and
/// each incoming text block, then join nonempty pieces with two line feeds.
fn append_native_tool_result_text(
    result: &mut Utf16JsonProjection,
    incoming: &[Utf16JsonProjection],
) -> bool {
    let Some(mut units) = result.string_units("") else {
        return false;
    };
    let mut pieces = Vec::with_capacity(incoming.len() + 1);
    let trimmed = trim_native_text_units(&units);
    if !trimmed.is_empty() {
        pieces.push(trimmed.to_vec());
    }
    for block in incoming {
        let Some(text) = block.string_units("/text") else {
            return false;
        };
        let text = trim_native_text_units(&text);
        if !text.is_empty() {
            pieces.push(text.to_vec());
        }
    }
    units.clear();
    for (index, piece) in pieces.into_iter().enumerate() {
        if index > 0 {
            units.extend([b'\n' as u16, b'\n' as u16]);
        }
        units.extend(piece);
    }
    let Ok(updated) = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
    else {
        return false;
    };
    *result = updated;
    true
}

/// Native `tMn` array-content path: if the tool result already ends in a text
/// block, append one LF to that block, then append incoming text blocks as
/// distinct array items. Protected media/search results keep the attachment
/// as a separate user block instead.
fn append_native_tool_result_text_blocks(
    result: &mut Utf16JsonProjection,
    incoming: &[Utf16JsonProjection],
) -> bool {
    let Some(len) = result.array_len("") else {
        return false;
    };
    let mut blocks = Vec::with_capacity(len + incoming.len());
    for index in 0..len {
        let pointer = format!("/{index}");
        let Ok(mut block) = result.subprojection(&pointer) else {
            return false;
        };
        if index + 1 == len && result.string_at(&format!("{pointer}/type")) == Some("text") {
            let Some(mut units) = result.string_units(&format!("{pointer}/text")) else {
                return false;
            };
            units.push(b'\n' as u16);
            let Ok(text) =
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            else {
                return false;
            };
            if block.set_field("text", text).is_err() {
                return false;
            }
        }
        let kind = result.string_at(&format!("{pointer}/type"));
        if matches!(kind, Some("image" | "document" | "search_result")) {
            return false;
        }
        blocks.push(block);
    }
    blocks.extend(incoming.iter().cloned());
    let Ok(updated) = Utf16JsonProjection::array(blocks) else {
        return false;
    };
    *result = updated;
    true
}

fn trim_native_text_units(units: &[u16]) -> &[u16] {
    fn is_native_whitespace(unit: u16) -> bool {
        matches!(
            unit,
            0x0009..=0x000d
                | 0x0020
                | 0x00a0
                | 0x1680
                | 0x2000..=0x200a
                | 0x2028..=0x2029
                | 0x202f
                | 0x205f
                | 0x3000
                | 0xfeff
        )
    }
    let start = units
        .iter()
        .position(|unit| !is_native_whitespace(*unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !is_native_whitespace(*unit))
        .map_or(start, |index| index + 1);
    &units[start..end]
}

/// Native adjacent API-system rows coalesce their text with two LF while
/// retaining block order and every contributing accepted message identity.
fn join_api_system_content(
    a: &mut Vec<ProtoBlock>,
    a_sources: &mut Vec<HashSet<lingxi_core::types::MessageId>>,
    mut b: Vec<ProtoBlock>,
    mut b_sources: Vec<HashSet<lingxi_core::types::MessageId>>,
) {
    if let (Some(left), Some(right)) = (a.last(), b.first()) {
        if matches!(
            left,
            ProtoBlock::Text { .. } | ProtoBlock::TextJsUtf16 { .. }
        ) && matches!(
            right,
            ProtoBlock::Text { .. } | ProtoBlock::TextJsUtf16 { .. }
        ) {
            let text_units = |block: &ProtoBlock| match block {
                ProtoBlock::Text { text, .. } => text.encode_utf16().collect::<Vec<_>>(),
                ProtoBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => utf16_code_units.clone(),
                _ => unreachable!("text seam contains text blocks"),
            };
            let mut units = text_units(left);
            units.extend([10, 10]);
            units.extend(text_units(right));
            let block = match String::from_utf16(&units) {
                Ok(text) => ProtoBlock::Text {
                    text,
                    citations: None,
                },
                Err(_) => ProtoBlock::TextJsUtf16 {
                    text: String::from_utf16_lossy(&units),
                    utf16_code_units: units,
                    citations: None,
                },
            };
            *a.last_mut().expect("joined text seam") = block;
            if let (Some(left_sources), Some(right_sources)) =
                (a_sources.last_mut(), b_sources.first())
            {
                left_sources.extend(right_sources);
            }
            b.remove(0);
            b_sources.remove(0);
        }
    }
    a.append(&mut b);
    a_sources.append(&mut b_sources);
}

/// Stable-partition `ToolResult` blocks to the front, preserving relative order
/// within each group.
///
/// Faithful port of claude-code `hoistToolResults` (`utils/messages.ts:2470`):
/// tool_result blocks must lead the user turn to avoid "tool result must follow
/// tool use" API errors.
fn hoist_tool_results(
    content: &mut Vec<ProtoBlock>,
    sources: &mut Vec<HashSet<lingxi_core::types::MessageId>>,
) {
    let mut paired = content.drain(..).zip(sources.drain(..)).collect::<Vec<_>>();
    paired.sort_by_key(|(block, _)| !matches!(block, ProtoBlock::ToolResult { .. }));
    for (block, block_sources) in paired {
        content.push(block);
        sources.push(block_sources);
    }
}

/// `ensureToolResultPairing` (claude-code `messages.ts:5133`): repair the
/// tool_use ↔ tool_result pairing of a message list before the wire, so a
/// resumed / interrupted / compacted transcript is not rejected by the API
/// (orphaned tool_result, missing tool_result, duplicate ids).
///
/// Complements the LOAD-time `recover_orphaned_parallel_tool_results`
/// (`session/jsonl/loader.rs`): this is the SEND-time pass that also catches
/// mid-session interrupts. Runs AFTER [`normalize_messages_for_api`]. On a CLEAN
/// turn — every `tool_use` has its matching `tool_result` in the following user
/// message, no duplicates, no orphans — this is a strict identity no-op.
///
/// Repairs (byte-faithful to the TS placeholders):
/// - Leading orphaned `tool_result`s (a user message with `tool_result` blocks
///   and no preceding assistant) are stripped; if that empties the first
///   message it becomes a `[Orphaned tool result removed due to conversation
///   resume]` text message.
/// - Duplicate `tool_use` ids (across messages) are de-duplicated; an orphaned
///   `server_tool_use` whose `advisor_tool_result` is missing is stripped; an
///   emptied assistant becomes a `[Tool use interrupted]` text message.
/// - A `tool_use` with no matching `tool_result` gets a synthetic error result
///   `[Tool result missing due to internal error]`; an orphaned/duplicate
///   `tool_result` is stripped.
#[must_use]
pub fn ensure_tool_result_pairing(messages: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
    ensure_tool_result_pairing_with_sources(ConversationMessagesWithSources::new(messages)).messages
}

pub(crate) fn ensure_tool_result_pairing_with_sources(
    messages: ConversationMessagesWithSources,
) -> ConversationMessagesWithSources {
    use lingxi_core::types::ContentBlock as B;
    use std::collections::{HashMap, HashSet};
    const SYNTH: &str = "[Tool result missing due to internal error]";
    const NO_CONTENT: &str = "(no content)";

    let mut result = ConversationMessagesWithSources {
        messages: Vec::with_capacity(messages.messages.len()),
        block_sources: Vec::with_capacity(messages.messages.len()),
    };
    let mut all_seen_tool_use_ids: HashSet<String> = HashSet::new();
    let mut i = 0usize;
    while i < messages.messages.len() {
        let msg = &messages.messages[i];
        let block_sources = &messages.block_sources[i];
        let ConversationMessage::Assistant {
            id: asst_id,
            content,
            stop_reason,
            resumable,
            per_turn_effort,
            ..
        } = msg
        else {
            if let ConversationMessage::User {
                id,
                content,
                is_meta,
                is_compact_summary,
                is_visible_in_transcript_only,
                turn_companion,
                usage_limit_note,
                origin,
                turn_origin,
                interrupted_by_shutdown,
                refused_message_id,
                api_message_override,
                ..
            } = msg
            {
                let prev_is_assistant = matches!(
                    result.messages.last(),
                    Some(ConversationMessage::Assistant { .. })
                );
                if !prev_is_assistant && content.iter().any(|b| matches!(b, B::ToolResult { .. })) {
                    let mut stripped = Vec::new();
                    let mut stripped_sources = Vec::new();
                    for (block, sources) in
                        content.iter().cloned().zip(block_sources.iter().cloned())
                    {
                        if !matches!(&block, B::ToolResult { .. }) {
                            stripped.push(block);
                            stripped_sources.push(sources);
                        }
                    }
                    if !stripped.is_empty() {
                        result.messages.push(ConversationMessage::User {
                            api_message_override: api_message_override.clone(),
                            id: *id,
                            content: stripped,
                            is_meta: *is_meta,
                            is_compact_summary: *is_compact_summary,
                            is_visible_in_transcript_only: *is_visible_in_transcript_only,
                            turn_companion: *turn_companion,
                            usage_limit_note: *usage_limit_note,
                            origin: origin.clone(),
                            turn_origin: turn_origin.clone(),
                            interrupted_by_shutdown: *interrupted_by_shutdown,
                            refused_message_id: refused_message_id.clone(),
                        });
                        result.block_sources.push(stripped_sources);
                    } else if result.messages.is_empty() {
                        let sources = block_sources
                            .iter()
                            .flat_map(|ids| ids.iter().copied())
                            .collect();
                        result.messages.push(ConversationMessage::user(
                            *id,
                            "[Orphaned tool result removed due to conversation resume]".into(),
                        ));
                        result.block_sources.push(vec![sources]);
                    }
                    i += 1;
                    continue;
                }
            }
            result.messages.push(msg.clone());
            result.block_sources.push(block_sources.clone());
            i += 1;
            continue;
        };

        let server_result_ids: HashSet<String> = content
            .iter()
            .filter_map(|b| match b {
                B::AdvisorToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();

        let mut seen_tool_use_ids: HashSet<String> = HashSet::new();
        let mut final_content: Vec<B> = Vec::with_capacity(content.len());
        let mut final_sources = Vec::with_capacity(block_sources.len());
        let mut tool_use_sources = HashMap::new();
        for (block, sources) in content.iter().zip(block_sources) {
            match block {
                B::ToolUse { id, .. } => {
                    let s = id.as_str().to_string();
                    if all_seen_tool_use_ids.contains(&s) {
                        continue;
                    }
                    all_seen_tool_use_ids.insert(s.clone());
                    seen_tool_use_ids.insert(s.clone());
                    tool_use_sources.insert(s, sources.clone());
                    final_content.push(block.clone());
                    final_sources.push(sources.clone());
                }
                B::ServerToolUse { id, .. } if !server_result_ids.contains(id) => {
                    continue;
                }
                _ => {
                    final_content.push(block.clone());
                    final_sources.push(sources.clone());
                }
            }
        }
        if final_content.is_empty() {
            let sources = block_sources
                .iter()
                .flat_map(|ids| ids.iter().copied())
                .collect();
            final_content.push(B::Text {
                text: "[Tool use interrupted]".into(),
                citations: Some(Some(
                    Utf16JsonProjection::parse("[]")
                        .expect("generated empty citation array is valid JSON"),
                )),
            });
            final_sources.push(sources);
        }
        result.messages.push(ConversationMessage::Assistant {
            id: *asst_id,
            content: final_content,
            stop_reason: stop_reason.clone(),
            resumable: resumable.clone(),
            per_turn_effort: per_turn_effort.clone(),
        });
        result.block_sources.push(final_sources);

        let next = messages.messages.get(i + 1);
        let next_sources = messages.block_sources.get(i + 1);
        let mut existing_tr_ids: HashSet<String> = HashSet::new();
        let mut has_dup_tr = false;
        if let Some(ConversationMessage::User { content, .. }) = next {
            for b in content {
                if let B::ToolResult { tool_use_id, .. } = b {
                    let t = tool_use_id.as_str().to_string();
                    if !existing_tr_ids.insert(t) {
                        has_dup_tr = true;
                    }
                }
            }
        }
        let missing: Vec<String> = seen_tool_use_ids
            .iter()
            .filter(|id| !existing_tr_ids.contains(*id))
            .cloned()
            .collect();
        let orphaned: HashSet<String> = existing_tr_ids
            .iter()
            .filter(|id| !seen_tool_use_ids.contains(*id))
            .cloned()
            .collect();

        if missing.is_empty() && orphaned.is_empty() && !has_dup_tr {
            i += 1;
            continue;
        }

        let synth: Vec<(B, HashSet<lingxi_core::types::MessageId>)> = missing
            .iter()
            .map(|id| {
                let sources = tool_use_sources.get(id).cloned().unwrap_or_default();
                let block = B::ToolResult {
                    tool_use_id: lingxi_core::types::ToolUseId::from(id.clone()),
                    content: Utf16JsonProjection::from(SYNTH),
                    is_error: Some(true),
                    provider_tool_use_id: None,
                    content_blocks: None,
                };
                (block, sources)
            })
            .collect();

        if let Some(ConversationMessage::User {
            id: uid,
            content,
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
            turn_companion,
            usage_limit_note,
            origin,
            turn_origin,
            refused_message_id,
            api_message_override: _,
            ..
        }) = next
        {
            let mut c = content
                .iter()
                .cloned()
                .zip(next_sources.into_iter().flatten().cloned())
                .collect::<Vec<_>>();
            if !orphaned.is_empty() || has_dup_tr {
                let mut seen: HashSet<String> = HashSet::new();
                c.retain(|(b, _)| match b {
                    B::ToolResult { tool_use_id, .. } => {
                        let t = tool_use_id.as_str().to_string();
                        if orphaned.contains(&t) {
                            return false;
                        }
                        seen.insert(t)
                    }
                    _ => true,
                });
            }
            let mut patched = synth;
            patched.extend(c);
            if !patched.is_empty() {
                result.messages.push(ConversationMessage::User {
                    api_message_override: None,
                    interrupted_by_shutdown: None,
                    refused_message_id: refused_message_id.clone(),
                    id: *uid,
                    content: patched.iter().map(|(block, _)| block.clone()).collect(),
                    is_meta: *is_meta,
                    is_compact_summary: *is_compact_summary,
                    is_visible_in_transcript_only: *is_visible_in_transcript_only,
                    turn_companion: *turn_companion,
                    usage_limit_note: *usage_limit_note,
                    origin: origin.clone(),
                    turn_origin: turn_origin.clone(),
                });
                result
                    .block_sources
                    .push(patched.into_iter().map(|(_, sources)| sources).collect());
            } else {
                // Role-alternation placeholder (claude-code `NO_CONTENT_MESSAGE`,
                // isMeta: true).
                result.messages.push(ConversationMessage::User {
                    api_message_override: None,
                    interrupted_by_shutdown: None,
                    refused_message_id: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![B::Text {
                        text: NO_CONTENT.to_string(),
                        citations: None,
                    }],
                    is_meta: true,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                });
                result.block_sources.push(vec![HashSet::new()]);
            }
            i += 2;
        } else {
            // Synthetic missing-result message (claude-code createUserMessage,
            // isMeta: true).
            if !synth.is_empty() {
                result.messages.push(ConversationMessage::User {
                    api_message_override: None,
                    interrupted_by_shutdown: None,
                    refused_message_id: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: synth.iter().map(|(block, _)| block.clone()).collect(),
                    is_meta: true,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                });
                result
                    .block_sources
                    .push(synth.into_iter().map(|(_, sources)| sources).collect());
            }
            i += 1;
        }
    }
    result
}

/// Lower Host tool wire definitions to provider-neutral runtime declarations.
/// The exact UTF-16 projection stays source-backed until the SDK codec emits it.
pub fn to_tool_declarations(
    tools: Vec<SourceToolWireDefinition>,
) -> Result<Vec<ToolDeclaration>, LlmError> {
    tools
        .into_iter()
        .map(convert_source_tool_declaration)
        .collect()
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn convert_message(msg: ConversationMessage) -> Result<Message, LlmError> {
    match msg {
        ConversationMessage::User { content, .. } => Ok(Message { api_output_config: None,
            role: "user".to_string(),
            content: content
                .into_iter()
                .map(convert_block)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ConversationMessage::Assistant { content, .. } => Ok(Message { api_output_config: None,
            role: "assistant".to_string(),
            content: content
                .into_iter()
                .map(convert_block)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ConversationMessage::System {
            api_system: Some(message),
            ..
        } => Ok(Message {
            role: "system".into(),
            content: message
                .content
                .into_iter()
                .map(convert_block)
                .collect::<Result<Vec<_>, _>>()?,
            api_output_config: message.output_config,
        }),
        ConversationMessage::System { .. } => Err(LlmError::InvalidRequest {
            message: "System messages must not appear in the messages vec; pass them via the system parameter".to_string(),
        }),
    }
}

fn convert_block(block: ProtoBlock) -> Result<LlmBlock, LlmError> {
    match block {
        ProtoBlock::ProviderContent { protocol, value } => Ok(LlmBlock::ProviderContent {
            protocol,
            value: projection_payload(&value, "provider content")?,
        }),
        ProtoBlock::Text { text, citations } => Ok(LlmBlock::Text {
            text,
            citations: projection_nullable_citations(citations)?,
            cache_control: None,
        }),
        ProtoBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations,
        } => Ok(LlmBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations: projection_nullable_citations(citations)?,
            cache_control: None,
        }),
        ProtoBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
            ..
        } => Ok(LlmBlock::ToolCall {
            // `id` IS the canonical provider-issued id (Anthropic `toolu_…`,
            // OpenAI `call_…`). `provider_id` is now vestigial/always None, so
            // this resolves to `id.to_string()` = the canonical id (byte parity).
            id: provider_id.unwrap_or_else(|| id.to_string()),
            name,
            input: projection_payload(&input, "tool input")?,
        }),
        ProtoBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            provider_tool_use_id,
            content_blocks,
            ..
        } => Ok(LlmBlock::ToolResult {
            // Must echo the same canonical id the paired `tool_use` carried so the
            // provider pairs them; `provider_tool_use_id` is vestigial/always None.
            tool_call_id: provider_tool_use_id.unwrap_or_else(|| tool_use_id.to_string()),
            // Keep either the exact result string or the structured block array
            // as raw JSON source for the SDK request boundary.
            output: projection_payload(
                content_blocks.as_ref().unwrap_or(&content),
                "tool result output",
            )?,
            is_error,
            cache_control: None,
            cache_reference: None,
        }),
        ProtoBlock::Thinking {
            thinking,
            signature,
        } => Ok(LlmBlock::Reasoning {
            text: thinking,
            signature,
        }),
        ProtoBlock::Image { source } => convert_image_source(source),
        ProtoBlock::Document { source } => convert_document_source(source),
        // Low-frequency server-side blocks: replayed verbatim into the next API
        // request so the provider round-trips them (protected-thinking/advisor/
        // connector betas). The Anthropic encoder round-trips RedactedThinking +
        // ServerToolUse and rejects ConnectorText/AdvisorToolResult on egress.
        ProtoBlock::RedactedThinking { data } => Ok(LlmBlock::RedactedThinking { data }),
        ProtoBlock::ServerToolUse { id, name, input } => Ok(LlmBlock::ServerToolUse {
            id,
            name,
            input: projection_payload(&input, "server tool input")?,
        }),
        ProtoBlock::ConnectorText {
            connector_text,
            signature,
        } => Ok(LlmBlock::ConnectorText {
            connector_text,
            signature,
        }),
        ProtoBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => Ok(LlmBlock::AdvisorToolResult {
            tool_use_id,
            content: projection_payload(&content, "advisor tool result")?,
            is_error,
        }),
        ProtoBlock::MediaAnalysis { analysis } => Ok(LlmBlock::Text {
            text: render_media_analysis(&analysis),
            citations: None,
            cache_control: None,
        }),
    }
}

fn projection_payload(
    projection: &Utf16JsonProjection,
    field: &str,
) -> Result<JsonPayload, LlmError> {
    let source = projection
        .to_json_string()
        .map_err(|error| LlmError::InvalidRequest {
            message: format!("{field} projection is invalid: {error}"),
        })?;
    JsonPayload::parse(&source).map_err(|error| LlmError::InvalidRequest {
        message: format!("{field} JSON is invalid: {error}"),
    })
}

fn projection_nullable_citations(
    citations: Option<Option<Utf16JsonProjection>>,
) -> Result<Option<Option<JsonPayload>>, LlmError> {
    citations
        .map(|citations| {
            citations
                .as_ref()
                .map(|projection| projection_payload(projection, "citation payload"))
                .transpose()
        })
        .transpose()
}

fn render_media_analysis(analysis: &MediaAnalysis) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "[Media analysis]");
    let _ = writeln!(out, "question_key: {}", analysis.question_key);
    let _ = writeln!(out, "model: {}", analysis.model);
    let _ = writeln!(out, "prompt_version: {}", analysis.prompt_version);
    let _ = writeln!(out, "truncated: {}", analysis.truncated);
    if !analysis.media_fingerprints.is_empty() {
        let _ = writeln!(
            out,
            "media_fingerprints: {}",
            analysis.media_fingerprints.join(", ")
        );
    }
    if !analysis.task_findings.is_empty() {
        let _ = writeln!(out, "task_findings:");
        for finding in &analysis.task_findings {
            let _ = writeln!(out, "- {}", finding);
        }
    }
    if !analysis.media.is_empty() {
        let _ = writeln!(out, "media:");
        for observation in &analysis.media {
            let _ = writeln!(out, "- {} ({})", observation.label, observation.fingerprint);
            let _ = writeln!(out, "  description: {}", observation.description);
            if let Some(ocr) = &observation.ocr {
                let _ = writeln!(out, "  ocr: {}", ocr);
            }
            if !observation.relevant_facts.is_empty() {
                let _ = writeln!(out, "  relevant_facts:");
                for fact in &observation.relevant_facts {
                    let _ = writeln!(out, "  - {}", fact);
                }
            }
            if let Some(uncertainty) = &observation.uncertainty {
                let _ = writeln!(out, "  uncertainty: {}", uncertainty);
            }
        }
    }
    if !analysis.cross_media_findings.is_empty() {
        let _ = writeln!(out, "cross_media_findings:");
        for finding in &analysis.cross_media_findings {
            let _ = writeln!(out, "- {}", finding);
        }
    }
    out.trim_end().to_string()
}

fn convert_image_source(source: ImageSource) -> Result<LlmBlock, LlmError> {
    match source {
        ImageSource::Base64 { media_type, data } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|e| LlmError::InvalidRequest {
                    message: format!("Image base64 decode failed: {e}"),
                })?;
            Ok(LlmBlock::Image { media_type, bytes })
        }
        ImageSource::Url { url } => Ok(LlmBlock::ImageUrl { url }),
    }
}

fn convert_document_source(source: DocumentSource) -> Result<LlmBlock, LlmError> {
    match source {
        DocumentSource::Base64 { media_type, data } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|e| LlmError::InvalidRequest {
                    message: format!("Document base64 decode failed: {e}"),
                })?;
            Ok(LlmBlock::Document { media_type, bytes })
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn convert_source_tool_declaration(
    source: SourceToolWireDefinition,
) -> Result<ToolDeclaration, LlmError> {
    let description = projection_payload(&source.description, "tool description")?;
    if !description.is_root_string() {
        return Err(LlmError::InvalidRequest {
            message: "tool description must be a JSON string value".into(),
        });
    }
    let input_schema = projection_payload(&source.input_schema, "tool input schema")?;

    Ok(ToolDeclaration {
        input_schema_projection: Some(source.input_schema),
        name: source.name,
        description,
        input_schema,
        strict: source.strict,
        defer_loading: source.defer_loading,
        ..Default::default()
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "convert_test.rs"]
mod convert_test;

#[cfg(test)]
#[path = "convert_tool_result_reminder_tests.rs"]
mod tool_result_reminder_tests;

/// Consume normalized durable history once into canonical SDK model input.
/// Exact JavaScript UTF-16 strings are retained outside model input for sealing.
pub use input_projection::history_input;
#[path = "history_input.rs"]
pub(crate) mod input_projection;
