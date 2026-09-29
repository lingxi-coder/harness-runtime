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
//! | `ContentBlock::ToolResult { tool_use_id, content, is_error }` | `ContentBlock::ToolResult { tool_call_id: …, output: Value::String(content), is_error, cache_control: None }` |
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
use protocol::{
    ContentBlock as ProtoBlock, ConversationMessage, DocumentSource, ImageSource, MediaAnalysis,
};
use serde_json::Value;

/// Convert a `Vec<ConversationMessage>` into `Vec<llm_runtime::Message>`.
///
/// Returns `Err(LlmError::InvalidRequest)` if any message is a `System`
/// variant — system prompts travel separately and must not appear in the
/// message vec.
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
    // Claude's query path first projects from the most recent compact boundary
    // (including the marker), then drops the transcript-only marker below.
    // Local compaction already replaces the active history, but SDK history
    // replay can deliver pre-boundary messages followed by the boundary, so the
    // slice is required here as a final model-facing invariant.
    let boundary_index = messages.iter().rposition(|message| {
        matches!(
            message,
            ConversationMessage::System {
                subtype: Some(subtype),
                ..
            } if subtype == "compact_boundary"
        ) || matches!(
            message,
            ConversationMessage::System {
                subtype: None,
                content,
                ..
            } if content == "Conversation compacted"
        )
    });
    let start = boundary_index.unwrap_or(0);
    let mut out: Vec<ConversationMessage> = Vec::with_capacity(messages.len() - start);
    for mut msg in messages.into_iter().skip(start) {
        // `stripAdvisorBlocks` (claude-code `claude.ts:1305`): drop
        // `advisor_tool_result` (no advisor beta) and `connector_text` (no
        // encode path — the Anthropic encoder rejects them) before the wire.
        // The blocks remain PRESERVED in the JSONL transcript (history); only
        // the outgoing clone is stripped. `redacted_thinking`/`server_tool_use`
        // are kept (they round-trip).
        match &mut msg {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => {
                content.retain(|b| {
                    !matches!(
                        b,
                        ProtoBlock::ConnectorText { .. } | ProtoBlock::AdvisorToolResult { .. }
                    )
                });
            }
            // Transcript-only markers (the `Conversation compacted` boundary,
            // `compaction/src/boundary.rs`) stay in `session.history` for JSONL
            // + TUI but MUST NOT reach the wire — the real system prompt rides
            // the `system` parameter and `convert_message` rejects any `System`
            // here. claude-code `isVisibleInTranscriptOnly`. Dropping pre-merge
            // also lets the surrounding same-role messages collapse below.
            ConversationMessage::System { .. } => continue,
        }
        if let ConversationMessage::User { content, .. } = &mut msg {
            normalize_tool_references(content, tool_search_enabled, available_tool_names);
        }
        match (out.last_mut(), msg) {
            (
                Some(ConversationMessage::User {
                    content: prev_content,
                    ..
                }),
                ConversationMessage::User {
                    content: new_content,
                    ..
                },
            ) => {
                join_text_at_seam(prev_content, new_content);
                hoist_tool_results(prev_content);
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
                ConversationMessage::Assistant {
                    content: new_content,
                    ..
                },
            ) => {
                join_text_at_seam(prev_content, new_content);
            }
            (_, msg) => out.push(msg),
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
        let had_reference = blocks.iter().any(is_tool_reference);
        if !had_reference {
            continue;
        }
        blocks.retain(|candidate| {
            if !is_tool_reference(candidate) {
                return true;
            }
            if !tool_search_enabled {
                return false;
            }
            // Oracle xPy: `let a=s.tool_name; if(!a)return!0; return t.has(W4(a))`.
            // A reference with a falsy tool_name (missing, non-string, or empty)
            // is KEPT; otherwise it survives iff its normalized name is available.
            let Some(name) = candidate
                .get("tool_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                return true;
            };
            available_tool_names.map_or(true, |available| {
                available.contains(normalize_legacy_tool_name(name))
            })
        });
        has_surviving_reference |= blocks.iter().any(is_tool_reference);
        if blocks.is_empty() {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": if tool_search_enabled {
                    UNAVAILABLE_PLACEHOLDER
                } else {
                    DISABLED_PLACEHOLDER
                },
            }));
        }
    }

    if tool_search_enabled
        && has_surviving_reference
        && !content.iter().any(
            |block| matches!(block, ProtoBlock::Text { text } if text.starts_with(TURN_BOUNDARY)),
        )
    {
        content.push(ProtoBlock::Text {
            text: TURN_BOUNDARY.to_string(),
        });
    }
}

/// Claude's persisted-tool alias normalization used when validating historical
/// tool_reference blocks against the current catalog.
/// The full 2.1.218 `W4`/`TOi` legacy tool-name alias map (12 entries). A
/// tool_reference whose (normalized) name is absent from the available set is
/// dropped, so every historical alias must be present or valid references get
/// silently stripped from the API-bound message.
fn normalize_legacy_tool_name(name: &str) -> &str {
    match name {
        "Task" => "Agent",
        "KillShell" | "KillBash" => "TaskStop",
        "AgentOutputTool" | "BashOutputTool" | "AgentOutput" | "BashOutput" => "TaskOutput",
        "ListPeers" => "ListAgents",
        "Brief" => "SendUserMessage",
        "ListMcpResources" => "ListMcpResourcesTool",
        "ReadMcpResource" => "ReadMcpResourceTool",
        "ReadMcpResourceDir" => "ReadMcpResourceDirTool",
        current => current,
    }
}

fn is_tool_reference(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("tool_reference")
}

/// Append `b` onto `a`, first joining a text|text seam with a `'\n'`.
///
/// Faithful port of claude-code `joinTextAtSeam` (`utils/messages.ts:2505`):
/// when `a`'s last block and `b`'s first block are both `Text`, the `'\n'` is
/// appended to `a`'s last text so no block's leading bytes change.
fn join_text_at_seam(a: &mut Vec<ProtoBlock>, mut b: Vec<ProtoBlock>) {
    if let (Some(ProtoBlock::Text { text: last }), Some(ProtoBlock::Text { .. })) =
        (a.last_mut(), b.first())
    {
        last.push('\n');
    }
    a.append(&mut b);
}

/// Stable-partition `ToolResult` blocks to the front, preserving relative order
/// within each group.
///
/// Faithful port of claude-code `hoistToolResults` (`utils/messages.ts:2470`):
/// tool_result blocks must lead the user turn to avoid "tool result must follow
/// tool use" API errors.
fn hoist_tool_results(content: &mut [ProtoBlock]) {
    // Stable sort on a boolean key = stable partition: `false` (tool_result)
    // sorts before `true` (everything else), and `sort_by_key` preserves the
    // relative order of equal-keyed elements within each group.
    content.sort_by_key(|b| !matches!(b, ProtoBlock::ToolResult { .. }));
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
    use protocol::ContentBlock as B;
    use std::collections::HashSet;
    const SYNTH: &str = "[Tool result missing due to internal error]";
    const NO_CONTENT: &str = "(no content)";

    let mut result: Vec<ConversationMessage> = Vec::with_capacity(messages.len());
    let mut all_seen_tool_use_ids: HashSet<String> = HashSet::new();
    let mut i = 0usize;
    while i < messages.len() {
        let msg = &messages[i];
        let ConversationMessage::Assistant {
            id: asst_id,
            content,
            stop_reason,
        } = msg
        else {
            if let ConversationMessage::User {
                id,
                content,
                is_meta,
                is_compact_summary,
                is_visible_in_transcript_only,
            } = msg
            {
                let prev_is_assistant =
                    matches!(result.last(), Some(ConversationMessage::Assistant { .. }));
                if !prev_is_assistant && content.iter().any(|b| matches!(b, B::ToolResult { .. })) {
                    let stripped: Vec<B> = content
                        .iter()
                        .filter(|b| !matches!(b, B::ToolResult { .. }))
                        .cloned()
                        .collect();
                    if !stripped.is_empty() {
                        result.push(ConversationMessage::User {
                            id: *id,
                            content: stripped,
                            is_meta: *is_meta,
                            is_compact_summary: *is_compact_summary,
                            is_visible_in_transcript_only: *is_visible_in_transcript_only,
                        });
                    } else if result.is_empty() {
                        result.push(ConversationMessage::user(
                            *id,
                            "[Orphaned tool result removed due to conversation resume]".into(),
                        ));
                    }
                    i += 1;
                    continue;
                }
            }
            result.push(msg.clone());
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
        for block in content {
            match block {
                B::ToolUse { id, .. } => {
                    let s = id.as_str().to_string();
                    if all_seen_tool_use_ids.contains(&s) {
                        continue;
                    }
                    all_seen_tool_use_ids.insert(s.clone());
                    seen_tool_use_ids.insert(s);
                    final_content.push(block.clone());
                }
                B::ServerToolUse { id, .. } if !server_result_ids.contains(id) => {
                    continue;
                }
                _ => final_content.push(block.clone()),
            }
        }
        if final_content.is_empty() {
            final_content.push(B::Text {
                text: "[Tool use interrupted]".into(),
            });
        }
        result.push(ConversationMessage::Assistant {
            id: *asst_id,
            content: final_content,
            stop_reason: stop_reason.clone(),
        });

        let next = messages.get(i + 1);
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

        let synth: Vec<B> = missing
            .iter()
            .map(|mid| B::ToolResult {
                tool_use_id: protocol::ToolUseId::from(mid.clone()),
                content: SYNTH.to_string(),
                is_error: true,
                provider_tool_use_id: None,
                content_blocks: None,
            })
            .collect();

        if let Some(ConversationMessage::User {
            id: uid,
            content,
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
        }) = next
        {
            let mut c = content.clone();
            if !orphaned.is_empty() || has_dup_tr {
                let mut seen: HashSet<String> = HashSet::new();
                c.retain(|b| match b {
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
                result.push(ConversationMessage::User {
                    id: *uid,
                    content: patched,
                    is_meta: *is_meta,
                    is_compact_summary: *is_compact_summary,
                    is_visible_in_transcript_only: *is_visible_in_transcript_only,
                });
            } else {
                // Role-alternation placeholder (claude-code `NO_CONTENT_MESSAGE`,
                // isMeta: true).
                result.push(ConversationMessage::User {
                    id: protocol::MessageId::new(),
                    content: vec![B::Text {
                        text: NO_CONTENT.to_string(),
                    }],
                    is_meta: true,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                });
            }
            i += 2;
        } else {
            // Synthetic missing-result message (claude-code createUserMessage,
            // isMeta: true).
            if !synth.is_empty() {
                result.push(ConversationMessage::User {
                    id: protocol::MessageId::new(),
                    content: synth,
                    is_meta: true,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                });
            }
            i += 1;
        }
    }
    result
}

/// Convert a `Vec<serde_json::Value>` (tool declarations in wire JSON shape)
/// into `Vec<llm_runtime::ToolDeclaration>`.
///
/// Each value must have string fields `name` and `description` and a
/// non-null `input_schema`; missing or wrong-typed fields produce
/// `Err(LlmError::InvalidRequest)` naming the offending field.
pub fn to_tool_declarations(tools: Vec<Value>) -> Result<Vec<ToolDeclaration>, LlmError> {
    tools.into_iter().map(convert_tool_declaration).collect()
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn convert_message(msg: ConversationMessage) -> Result<Message, LlmError> {
    match msg {
        ConversationMessage::User { content, .. } => Ok(Message {
            role: "user".to_string(),
            content: content
                .into_iter()
                .map(convert_block)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ConversationMessage::Assistant { content, .. } => Ok(Message {
            role: "assistant".to_string(),
            content: content
                .into_iter()
                .map(convert_block)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ConversationMessage::System { .. } => Err(LlmError::InvalidRequest {
            message: "System messages must not appear in the messages vec; pass them via the system parameter".to_string(),
        }),
    }
}

fn convert_block(block: ProtoBlock) -> Result<LlmBlock, LlmError> {
    match block {
        ProtoBlock::ProviderContent { protocol, value } => {
            Ok(LlmBlock::ProviderContent { protocol, value })
        }
        ProtoBlock::Text { text } => Ok(LlmBlock::Text {
            text,
            cache_control: None,
        }),
        ProtoBlock::TextJsUtf16 {
            text,
            utf16_code_units,
        } => Ok(LlmBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            cache_control: None,
        }),
        ProtoBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
        } => Ok(LlmBlock::ToolCall {
            // `id` IS the canonical provider-issued id (Anthropic `toolu_…`,
            // OpenAI `call_…`). `provider_id` is now vestigial/always None, so
            // this resolves to `id.to_string()` = the canonical id (byte parity).
            id: provider_id.unwrap_or_else(|| id.to_string()),
            name,
            input,
        }),
        ProtoBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            provider_tool_use_id,
            content_blocks,
        } => Ok(LlmBlock::ToolResult {
            // Must echo the same canonical id the paired `tool_use` carried so the
            // provider pairs them; `provider_tool_use_id` is vestigial/always None.
            tool_call_id: provider_tool_use_id.unwrap_or_else(|| tool_use_id.to_string()),
            // A structured content-block array (MCP image/resource) rides as the
            // `Value::Array` output (emitted verbatim); plain text stays a String.
            output: content_blocks.map_or_else(|| Value::String(content), Value::Array),
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
        ProtoBlock::ServerToolUse { id, name, input } => {
            Ok(LlmBlock::ServerToolUse { id, name, input })
        }
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
            content,
            is_error,
        }),
        ProtoBlock::MediaAnalysis { analysis } => Ok(LlmBlock::Text {
            text: render_media_analysis(&analysis),
            cache_control: None,
        }),
    }
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
fn convert_tool_declaration(value: Value) -> Result<ToolDeclaration, LlmError> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::InvalidRequest {
            message: "Tool declaration missing required string field: name".to_string(),
        })?
        .to_string();

    let description = value
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::InvalidRequest {
            message: "Tool declaration missing required string field: description".to_string(),
        })?
        .to_string();

    let input_schema = value
        .get("input_schema")
        .cloned()
        .filter(|v| !v.is_null())
        .ok_or_else(|| LlmError::InvalidRequest {
            message: "Tool declaration missing required field: input_schema".to_string(),
        })?;

    // Structured-output strict mode: a wire tool may carry `"strict": true`
    // (set by its producer when `tengu_structured_output_strict` is on). Absent
    // ⇒ `false`, so a normal tool is unaffected.
    let strict = value
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let defer_loading = value
        .get("defer_loading")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(ToolDeclaration {
        name,
        description,
        input_schema,
        strict,
        defer_loading,
        ..Default::default()
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "convert_test.rs"]
mod convert_test;

/// Consume normalized durable history once into canonical SDK model input.
/// Exact JavaScript UTF-16 strings are retained outside model input for sealing.
pub use input_projection::history_input;
#[path = "history_input.rs"]
pub(crate) mod input_projection;
