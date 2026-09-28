//! Message and transcript conversion.
use super::value_to_json_string;
use crate::protocol::message::{MessageBlockDto, MessageDto, MessageImageDto};
use protocol::ConversationMessage;

/// Lower one [`ConversationMessage`] to a [`MessageDto`] — the resumed-scrollback
/// twin of the per-turn [`crate::adapter::turn::synthesize_message`].
///
/// The role is the message's wire role (`"user"` / `"assistant"` / `"system"`);
/// the content blocks are lowered through the SAME
/// [`crate::adapter::turn::lower_content_block`] path `MessageComplete` uses, so a resumed
/// message and a live-turn message reproduce an IDENTICAL [`MessageDto`] block set
/// for any given content. Image content is projected to the message-level
/// `images` field because clients render it before the user's text row; document
/// content has no client message projection and is dropped.
///
/// A [`ConversationMessage::System`] carries a flat `content: String` (no blocks),
/// so it lowers to a single [`MessageBlockDto::Text`] — a faithful, lossless
/// scrollback rendering of the system body.
#[must_use]
pub fn lower_conversation_message(message: &ConversationMessage) -> MessageDto {
    lower_conversation_message_with(message, &mut crate::adapter::turn::ToolUseIndex::default())
}

/// [`lower_conversation_message`] threading a [`crate::adapter::turn::ToolUseIndex`] so a
/// `ToolResult` can be paired with the `ToolUse` from the PREVIOUS message.
///
/// Prefer this whenever more than one message is lowered: a tool call and its
/// result are always in adjacent messages, never the same one, so a per-message
/// index can never pair them.
#[must_use]
pub fn lower_conversation_message_with(
    message: &ConversationMessage,
    index: &mut crate::adapter::turn::ToolUseIndex,
) -> MessageDto {
    match message {
        ConversationMessage::User { content, .. } => {
            let blocks = legacy_cron_slash_line(content).map_or_else(
                || {
                    content
                        .iter()
                        .filter_map(|block| {
                            crate::adapter::turn::lower_content_block_with(block, index)
                        })
                        .collect()
                },
                |text| vec![MessageBlockDto::Text { text }],
            );
            MessageDto {
                loop_wakeup: None,
                role: "user".to_string(),
                blocks,
                images: content.iter().filter_map(lower_message_image).collect(),
            }
        }
        ConversationMessage::Assistant { content, .. } => MessageDto {
            loop_wakeup: None,
            role: "assistant".to_string(),
            blocks: content
                .iter()
                .filter_map(|block| crate::adapter::turn::lower_content_block_with(block, index))
                .collect(),
            images: Vec::new(),
        },
        ConversationMessage::System {
            content, subtype, ..
        } if subtype.as_deref() == Some("scheduled_task_fire") => {
            let payload: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
            if payload.get("taskKindLoop") == Some(&serde_json::json!(false)) {
                MessageDto {
                    role: "system".into(),
                    images: Vec::new(),
                    loop_wakeup: None,
                    blocks: vec![MessageBlockDto::Text {
                        text: payload["message"].as_str().unwrap_or_default().into(),
                    }],
                }
            } else {
                MessageDto {
                    role: "system".into(),
                    blocks: Vec::new(),
                    images: Vec::new(),
                    loop_wakeup: serde_json::from_value(payload).ok(),
                }
            }
        }
        ConversationMessage::System {
            subtype,
            compact_metadata,
            ..
        } if subtype.as_deref() == Some("compact_boundary") => MessageDto {
            loop_wakeup: None,
            role: "system".to_string(),
            blocks: vec![MessageBlockDto::CompactBoundary {
                messages_before: compact_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.messages_summarized)
                    .unwrap_or_default(),
                messages_after: 0,
                summary: String::new(),
            }],
            images: Vec::new(),
        },
        ConversationMessage::System { content, .. } => MessageDto {
            loop_wakeup: None,
            role: "system".to_string(),
            blocks: vec![MessageBlockDto::Text {
                text: content.clone(),
            }],
            images: Vec::new(),
        },
    }
}

/// Older `/cron` command bundles persisted their expanded internal prompt but
/// only echoed the original slash line in the live client. Recover the exact
/// user arguments for resumed scrollback without changing the history that is
/// sent back to the model.
fn legacy_cron_slash_line(content: &[protocol::ContentBlock]) -> Option<String> {
    const PREFIX: &str = "The user explicitly invoked `/cron` to manage scheduled prompts.";
    const ARGUMENTS_MARKER: &str = "\nArguments: ";

    let [protocol::ContentBlock::Text { text }] = content else {
        return None;
    };
    if !text.starts_with(PREFIX) {
        return None;
    }
    let serialized = text.rsplit_once(ARGUMENTS_MARKER)?.1.trim();
    let arguments: String = serde_json::from_str(serialized).ok()?;
    let arguments = arguments.trim();
    Some(if arguments.is_empty() {
        "/cron".to_string()
    } else {
        format!("/cron {arguments}")
    })
}

/// Keep persisted image bytes renderable across every client. The engine's
/// session history is already durable, so inline data becomes a URL-shaped
/// transcript value; a source that was already a URL remains a URL.
fn lower_message_image(block: &protocol::ContentBlock) -> Option<MessageImageDto> {
    let protocol::ContentBlock::Image { source } = block else {
        return None;
    };
    match source {
        protocol::ImageSource::Base64 { media_type, data } => Some(MessageImageDto {
            media_type: media_type.clone(),
            url: format!("data:{media_type};base64,{data}"),
        }),
        protocol::ImageSource::Url { url } => Some(MessageImageDto {
            media_type: String::new(),
            url: url.clone(),
        }),
    }
}

/// Lower a replayed conversation `history` to the OLDEST-FIRST [`MessageDto`]
/// transcript carried by [`ClientEvent::SessionResumed`](crate::protocol::events::ClientEvent::SessionResumed).
///
/// `history` is already in chronological (oldest-first) order — the engine's
/// resume path replays the JSONL in file order — so this preserves that order
/// 1:1. Each message lowers through [`lower_conversation_message`], reusing the
/// same `ContentBlock` → `MessageBlockDto` rules as the live `MessageComplete`
/// path so the resumed scrollback is byte-identical to what a live turn would
/// have produced.
#[must_use]
pub fn lower_transcript(history: &[ConversationMessage]) -> Vec<MessageDto> {
    lower_transcript_with_tool_results(history, &std::collections::HashMap::new())
}

/// [`lower_transcript`] with the client-state tools' structured payloads restored.
///
/// `ContentBlock::ToolResult` keeps only the model-facing text, so a replayed
/// call loses everything its live `ToolUseResult` carried in `data` — the
/// `agentId` that links a subagent card to the call that spawned it, and the
/// `plan`/`model_content` that give a plan card its document and its approval.
/// `tool_results` is
/// [`orchestrator::resume::ReplayedSession::client_state_tool_results`]: those
/// payloads keyed by `tool_use_id`, already restricted to
/// `orchestrator::CLIENT_STATE_TOOLS`.
///
/// Deliberately NOT every tool's payload. A tool's structured `data` is the
/// uncapped raw result — `Read` carries an image's base64, `Edit` carries the
/// whole pre-edit file — none of which renders (`display` is derived from the
/// text, and this DTO has no `content_blocks`), while the model-facing text it
/// would replace is the one `tool_result_persistence` already capped. Restoring
/// all of them measured 2.0x-34.5x on real transcripts for a payload that ships
/// as a single `SessionResumed` frame; restoring only the allowlisted ones is
/// 1.00x-1.04x.
#[must_use]
pub fn lower_transcript_with_tool_results<S: std::hash::BuildHasher>(
    history: &[ConversationMessage],
    tool_results: &std::collections::HashMap<String, serde_json::Value, S>,
) -> Vec<MessageDto> {
    let mut transcript = lower_transcript_inner(history);
    // A post-pass rather than a hook inside `lower_content_block_with`: that
    // function is on the LIVE `MessageComplete` path too, and nothing about
    // this override needs the pairing index — the lowered block already carries
    // the `id` to look up and the `result_json` to replace.
    if !tool_results.is_empty() {
        for block in transcript
            .iter_mut()
            .flat_map(|message| &mut message.blocks)
        {
            if let MessageBlockDto::ToolResult {
                id, result_json, ..
            } = block
            {
                if let Some(data) = tool_results.get(id.as_str()) {
                    *result_json = value_to_json_string(data);
                }
            }
        }
    }
    transcript
}

fn lower_transcript_inner(history: &[ConversationMessage]) -> Vec<MessageDto> {
    let mut transcript = Vec::with_capacity(history.len());
    // ONE index for the WHOLE transcript: a `ToolUse` in assistant message N
    // pairs with its `ToolResult` in user message N+1.
    let mut tool_uses = crate::adapter::turn::ToolUseIndex::default();
    let mut pending_loop_companion: Option<String> = None;
    for message in history {
        if let Some(expected) = pending_loop_companion.take() {
            if let ConversationMessage::User {
                content,
                is_meta: true,
                ..
            } = message
            {
                let text = content
                    .iter()
                    .filter_map(|block| match block {
                        protocol::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text == expected {
                    continue;
                }
            }
        }
        if let ConversationMessage::System {
            content, subtype, ..
        } = message
        {
            if subtype.as_deref() == Some("scheduled_task_fire") {
                pending_loop_companion = serde_json::from_str::<serde_json::Value>(content)
                    .ok()
                    .and_then(|value| value["companion"].as_str().map(str::to_owned));
            }
        }
        match message {
            ConversationMessage::User {
                content,
                is_compact_summary: true,
                ..
            } => {
                let summary = content
                    .iter()
                    .filter_map(|block| match block {
                        protocol::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let Some(MessageDto { blocks, .. }) = transcript.last_mut() else {
                    continue;
                };
                let Some(MessageBlockDto::CompactBoundary {
                    summary: accumulated,
                    ..
                }) = blocks.last_mut()
                else {
                    continue;
                };
                if !summary.is_empty() {
                    if !accumulated.is_empty() {
                        accumulated.push('\n');
                    }
                    accumulated.push_str(&summary);
                }
            }
            ConversationMessage::User {
                is_visible_in_transcript_only: true,
                ..
            } => {}
            ConversationMessage::User {
                content,
                is_meta: true,
                ..
            } => {
                // Scheduled inputs and other internal meta text are model context,
                // not user-authored scrollback. Tool results still belong to their
                // visible calls and must retain the shared transcript index.
                let blocks: Vec<_> = content
                    .iter()
                    .filter(|block| matches!(block, protocol::ContentBlock::ToolResult { .. }))
                    .filter_map(|block| {
                        crate::adapter::turn::lower_content_block_with(block, &mut tool_uses)
                    })
                    .collect();
                if !blocks.is_empty() {
                    transcript.push(MessageDto {
                        role: "user".to_string(),
                        blocks,
                        images: Vec::new(),
                        loop_wakeup: None,
                    });
                }
            }
            _ => transcript.push(lower_conversation_message_with(message, &mut tool_uses)),
        }
    }
    transcript
}
