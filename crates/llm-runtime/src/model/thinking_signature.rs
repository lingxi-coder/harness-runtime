//! Thinking-signature 400 recovery (claude-code 2.1.259 `_ot` / `Zz` / `CCt` / `wkt`).
//!
//! A 400 whose body matches thinking-signature copy is healed by stripping
//! `thinking` / `redacted_thinking` (and empty text) from assistant messages
//! and retrying once. The latch is session-scoped so later turns do not resend
//! the rejected blocks.
//!
//! LingXi is multi-provider: OpenAI, Gemini, Claude, and OpenAI-compat proxies
//! all support thinking. Immediate retry is gated by the **error classifier**,
//! not protocol family. Later-turn latch strip applies on every thinking
//! model except DeepSeek / Kimi, which 400 if `reasoning_content` is omitted.

use crate::{ContentBlock, Message};

/// `true` when later turns must keep assistant thinking / `reasoning_content`.
///
/// DeepSeek and Kimi 400 if a thinking-mode turn omits `reasoning_content`.
/// Other thinking-capable models (OpenAI, Gemini, Claude, OpenAI-compat
/// proxies) do not have that round-trip requirement, so a thinking-signature
/// latch may strip outbound thinking.
#[must_use]
pub fn thinking_must_round_trip(model: &str, profile: Option<&str>) -> bool {
    let haystack = format!("{} {}", profile.unwrap_or(""), model).to_ascii_lowercase();
    haystack.contains("deepseek") || haystack.contains("kimi")
}

/// Claude Code `_ot` + `lb(..., "thinking_signature")` classifier.
///
/// Status 400 is the caller's job ([`crate::LlmError::InvalidRequest`]). This
/// matches the message body only, so DeepSeek / OpenAI / Gemini 400 copy that
/// talks about `reasoning_content` or generic validation cannot fire it.
#[must_use]
pub fn is_thinking_signature_rejection(message: &str) -> bool {
    if message.contains("thinking_signature") {
        return true;
    }
    let normalized = message.to_lowercase().replace('`', "");
    if normalized.contains("signature in thinking block") {
        return true;
    }
    if normalized.contains("thinking.signature") && normalized.contains("field required") {
        return true;
    }
    let thinking_block =
        normalized.contains("thinking block") || normalized.contains("redacted_thinking");
    thinking_block
        && (normalized.contains("cannot be modified") || normalized.contains("invalid signature"))
}

/// Count signed vs unsigned thinking blocks on assistant messages.
///
/// Signed: `redacted_thinking`, or `thinking`/`Reasoning` with a non-empty
/// signature. Unsigned: `Reasoning` without a signature. Used for
/// `tengu_thinking_signature_strip_retry`.
#[must_use]
pub fn count_thinking_signature_blocks(messages: &[Message]) -> (u32, u32) {
    let mut signed = 0u32;
    let mut unsigned = 0u32;
    for message in messages
        .iter()
        .filter(|message| message.role == "assistant")
    {
        for block in &message.content {
            match block {
                ContentBlock::RedactedThinking { .. } => signed += 1,
                ContentBlock::Reasoning { signature, .. } => {
                    if signature.as_deref().is_some_and(|value| !value.is_empty()) {
                        signed += 1;
                    } else {
                        unsigned += 1;
                    }
                }
                _ => {}
            }
        }
    }
    (signed, unsigned)
}

fn text_payload(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => Some(text),
        _ => None,
    }
}

fn is_thinking_block(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Reasoning { .. } | ContentBlock::RedactedThinking { .. }
    )
}

/// `CCt(e, 0)` / `wkt`: drop assistant `thinking` / `redacted_thinking` and
/// empty/whitespace text. If a message would become empty, insert
/// `[Thinking removed]`. Returns whether any message changed.
pub fn strip_thinking_blocks_for_signature_recovery(messages: &mut [Message]) -> bool {
    let mut changed = false;
    for message in messages
        .iter_mut()
        .filter(|message| message.role == "assistant")
    {
        if !message.content.iter().any(is_thinking_block) {
            continue;
        }
        message.content.retain(|block| {
            if is_thinking_block(block) {
                return false;
            }
            match text_payload(block) {
                Some(text) => !text.trim().is_empty(),
                None => true,
            }
        });
        if message.content.is_empty() {
            message.content.push(ContentBlock::Text {
                text: "[Thinking removed]".into(),
                cache_control: None,
            });
        }
        changed = true;
    }
    changed
}

/// Apply Claude's CCt to identity-scoped historical blocks before normalization.
/// Partial markers preserve the prefix before the selected thinking block.
pub fn strip_marked_conversation_thinking(
    messages: &mut [lingxi_core::types::ConversationMessage],
    marked: &std::collections::HashMap<lingxi_core::types::MessageId, usize>,
) {
    use lingxi_core::types::{ContentBlock as Block, ConversationMessage};
    for message in messages {
        let ConversationMessage::Assistant { id, content, .. } = message else {
            continue;
        };
        let Some(&from) = marked.get(id) else {
            continue;
        };
        let is_thinking = |block: &Block| {
            matches!(
                block,
                Block::Thinking { .. } | Block::RedactedThinking { .. }
            )
        };
        let Some(start) = content
            .iter()
            .enumerate()
            .filter(|(_, block)| is_thinking(block))
            .nth(from)
            .map(|(index, _)| index)
        else {
            continue;
        };
        let mut index = 0;
        content.retain(|block| {
            let preserve_prefix = from > 0 && index < start;
            index += 1;
            preserve_prefix
                || (!is_thinking(block)
                    && !matches!(block, Block::Text { text } if text.trim().is_empty()))
        });
        if content.is_empty() {
            content.push(Block::Text {
                text: "[Thinking removed]".into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn partial_marker_preserves_prefix_and_fresh_assistant_thinking() {
        use lingxi_core::types::{ContentBlock as Block, ConversationMessage, MessageId};
        let old = MessageId::new();
        let fresh = MessageId::new();
        let thinking = || Block::Thinking {
            thinking: "same".into(),
            signature: Some("sig".into()),
        };
        let mut messages = vec![
            ConversationMessage::Assistant {
                id: old,
                content: vec![
                    thinking(),
                    Block::Text { text: " ".into() },
                    thinking(),
                    Block::Text { text: "\n".into() },
                ],
                stop_reason: None,
            },
            ConversationMessage::Assistant {
                id: fresh,
                content: vec![thinking()],
                stop_reason: None,
            },
        ];
        strip_marked_conversation_thinking(&mut messages, &[(old, 1)].into_iter().collect());
        match &messages[0] {
            ConversationMessage::Assistant { content, .. } => {
                assert_eq!(content, &vec![thinking(), Block::Text { text: " ".into() }])
            }
            _ => unreachable!(),
        }
        match &messages[1] {
            ConversationMessage::Assistant { content, .. } => {
                assert_eq!(content, &vec![thinking()])
            }
            _ => unreachable!(),
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: "assistant".into(),
            content,
        }
    }

    fn user(content: Vec<ContentBlock>) -> Message {
        Message {
            role: "user".into(),
            content,
        }
    }

    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.into(),
            cache_control: None,
        }
    }

    fn reasoning(body: &str, signature: Option<&str>) -> ContentBlock {
        ContentBlock::Reasoning {
            text: body.into(),
            signature: signature.map(str::to_string),
        }
    }

    #[test]
    fn classifier_matches_anthropic_thinking_signature_copy() {
        let hits = [
            "Invalid signature in thinking block",
            "messages.0.content.0.thinking.signature: Field required",
            "`thinking.signature`: Field required",
            "`thinking` block cannot be modified",
            "redacted_thinking cannot be modified",
            "thinking block has invalid signature",
            "capability_rejected: thinking_signature",
        ];
        for message in hits {
            assert!(
                is_thinking_signature_rejection(message),
                "expected hit: {message}"
            );
        }
    }

    #[test]
    fn classifier_rejects_non_anthropic_and_generic_400_copy() {
        let misses = [
            "The reasoning_content in the thinking mode must be passed back to the API.",
            "some other validation error",
            "input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000",
            "Fast mode is not enabled",
            "This model does not support the effort parameter",
        ];
        for message in misses {
            assert!(
                !is_thinking_signature_rejection(message),
                "expected miss: {message}"
            );
        }
    }

    #[test]
    fn thinking_must_round_trip_only_for_deepseek_and_kimi() {
        assert!(thinking_must_round_trip(
            "deepseek-reasoner",
            Some("deepseek")
        ));
        assert!(thinking_must_round_trip("kimi-k2", Some("kimi")));
        assert!(thinking_must_round_trip("moonshot-kimi", None));
        assert!(!thinking_must_round_trip("gpt-4o", Some("openai")));
        assert!(!thinking_must_round_trip(
            "gemini-2.5-flash",
            Some("gemini")
        ));
        assert!(!thinking_must_round_trip("claude-sonnet-4-20250514", None));
    }

    #[test]
    fn strip_drops_assistant_thinking_and_empty_text_keeps_tools_and_user() {
        let mut messages = vec![
            assistant(vec![
                reasoning("secret", Some("sig")),
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
                text("   "),
                text("keep me"),
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "Echo".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ConnectorText {
                    connector_text: "connector".into(),
                    signature: Some("sig".into()),
                },
            ]),
            user(vec![reasoning("leave user reasoning", Some("sig"))]),
        ];

        assert!(strip_thinking_blocks_for_signature_recovery(&mut messages));
        assert_eq!(
            messages[0].content,
            vec![
                text("keep me"),
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "Echo".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ConnectorText {
                    connector_text: "connector".into(),
                    signature: Some("sig".into()),
                },
            ]
        );
        assert!(matches!(
            messages[1].content.as_slice(),
            [ContentBlock::Reasoning { .. }]
        ));
    }

    #[test]
    fn strip_inserts_placeholder_when_assistant_would_be_empty() {
        let mut messages = vec![assistant(vec![reasoning("only thinking", Some("sig"))])];
        assert!(strip_thinking_blocks_for_signature_recovery(&mut messages));
        assert_eq!(messages[0].content, vec![text("[Thinking removed]")]);
    }

    #[test]
    fn strip_is_identity_when_no_thinking_present() {
        let mut messages = vec![assistant(vec![text("hello")])];
        assert!(!strip_thinking_blocks_for_signature_recovery(&mut messages));
        assert_eq!(messages[0].content, vec![text("hello")]);
    }

    #[test]
    fn counts_signed_redacted_and_unsigned_thinking() {
        let messages = vec![assistant(vec![
            reasoning("signed", Some("sig")),
            reasoning("unsigned", None),
            ContentBlock::RedactedThinking {
                data: "opaque".into(),
            },
            text("keep"),
        ])];
        assert_eq!(count_thinking_signature_blocks(&messages), (2, 1));
    }
}

/// Count model-facing SDK thinking blocks for retry accounting.
pub fn count_input_thinking(
    messages: &[lingxi_llm_client::protocol::ConversationMessage],
) -> (u32, u32) {
    use lingxi_llm_client::protocol::{ContentBlock as Block, MessageRole};
    let mut counts = (0, 0);
    for block in messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| &m.content)
    {
        match block {
            Block::RedactedThinking { .. } => counts.0 += 1,
            Block::Thinking { signature, .. }
                if signature.as_deref().is_some_and(|s| !s.is_empty()) =>
            {
                counts.0 += 1
            }
            Block::Thinking { .. } => counts.1 += 1,
            _ => {}
        }
    }
    counts
}

/// Retain canonical input blocks while keeping positional cache and exact-string
/// sidecars attached to the same surviving blocks. Empty messages are preserved;
/// a caller may explicitly request a placeholder for messages made empty.
pub(crate) fn retain_input_blocks(
    request: &mut crate::LlmRequest,
    mut retain: impl FnMut(usize, &lingxi_llm_client::protocol::ContentBlock) -> bool,
    empty_placeholder: Option<&str>,
) -> bool {
    use lingxi_llm_client::protocol::{CachePosition, ContentBlock as Block};
    let previous = std::mem::take(&mut request.execution.message_json_string_overrides);
    let mut remapped = previous.clone();
    let mut changed = false;
    let mut positions = std::collections::BTreeMap::new();
    let mut affected = std::collections::HashSet::new();
    for (mi, message) in request.input.messages.iter_mut().enumerate() {
        let retained: Vec<_> = message
            .content
            .iter()
            .map(|block| retain(mi, block))
            .collect();
        if retained.iter().all(|keep| *keep) {
            continue;
        }
        affected.insert(mi);
        let prefix = format!("/messages/{mi}/content/");
        remapped.retain(|key, _| !key.starts_with(&prefix));
        let old = std::mem::take(&mut message.content);
        let old_len = old.len();
        for (bi, block) in old.into_iter().enumerate() {
            if !retained[bi] {
                continue;
            }
            positions.insert((mi, bi), message.content.len());
            let old_prefix = format!("{prefix}{bi}/");
            let new_prefix = format!("{prefix}{}/", message.content.len());
            for (key, units) in &previous {
                if let Some(suffix) = key.strip_prefix(&old_prefix) {
                    remapped.insert(format!("{new_prefix}{suffix}"), units.clone());
                }
            }
            message.content.push(block);
        }
        let message_changed = message.content.len() != old_len;
        changed |= message_changed;
        if message_changed && message.content.is_empty() {
            if let Some(text) = empty_placeholder {
                message.content.push(Block::Text {
                    text: text.into(),
                    thought_signature: None,
                });
            }
        }
    }
    request
        .input
        .prompt_cache
        .breakpoints
        .retain_mut(|breakpoint| {
            if let CachePosition::Message { index, block } = &mut breakpoint.position {
                if !affected.contains(index) {
                    return true;
                }
                let Some(new_block) = positions.get(&(*index, *block)) else {
                    return false;
                };
                *block = *new_block;
            }
            true
        });
    request.execution.message_json_string_overrides = remapped;
    changed
}

/// Remove rejected SDK thinking while remapping positional sidecars.
pub fn strip_input_thinking(request: &mut crate::LlmRequest) -> bool {
    use lingxi_llm_client::protocol::{ContentBlock as Block, MessageRole};
    let is_thinking = |block: &Block| {
        matches!(
            block,
            Block::Thinking { .. } | Block::RedactedThinking { .. }
        )
    };
    let affected: Vec<_> = request
        .input
        .messages
        .iter()
        .map(|message| {
            message.role == MessageRole::Assistant && message.content.iter().any(is_thinking)
        })
        .collect();
    retain_input_blocks(
        request,
        |index, block| {
            !affected[index]
                || !(is_thinking(block)
                    || matches!(block, Block::Text { text, .. } if text.trim().is_empty()))
        },
        Some("[Thinking removed]"),
    )
}

#[cfg(test)]
mod input_recovery_tests {
    use super::*;
    use lingxi_llm_client::protocol::{ContentBlock as Block, ConversationMessage, MessageRole};

    #[test]
    fn capability_degradation_preserves_whitespace_and_empty_messages() {
        let mut request = crate::LlmRequest::new("model");
        request.input.messages = vec![
            ConversationMessage {
                role: MessageRole::Assistant,
                content: vec![
                    Block::Thinking {
                        text: "hidden".into(),
                        signature: None,
                    },
                    Block::Text {
                        text: "  ".into(),
                        thought_signature: None,
                    },
                ],
                native_options: vec![],
            },
            ConversationMessage {
                role: MessageRole::Assistant,
                content: vec![Block::Thinking {
                    text: "hidden".into(),
                    signature: None,
                }],
                native_options: vec![],
            },
        ];
        assert!(retain_input_blocks(
            &mut request,
            |_, block| !matches!(block, Block::Thinking { .. }),
            None
        ));
        assert!(
            matches!(&request.input.messages[0].content[0], Block::Text { text, .. } if text == "  ")
        );
        assert!(request.input.messages[1].content.is_empty());
    }

    #[test]
    fn canonical_recovery_remaps_exact_string_sidecar_after_removing_thinking() {
        let mut request = crate::LlmRequest::new("model");
        request.input.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            content: vec![
                Block::Thinking {
                    text: "hidden".into(),
                    signature: Some("bad".into()),
                },
                Block::Text {
                    text: "�".into(),
                    thought_signature: None,
                },
            ],
            native_options: vec![],
        });
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/1/text".into(), vec![0xd800]);
        request
            .input
            .prompt_cache
            .breakpoints
            .push(lingxi_llm_client::protocol::CacheBreakpoint {
                position: lingxi_llm_client::protocol::CachePosition::Message {
                    index: 0,
                    block: 1,
                },
                ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
                scope: None,
            });
        assert_eq!(count_input_thinking(&request.input.messages), (1, 0));
        assert!(strip_input_thinking(&mut request));
        assert_eq!(request.input.messages[0].content.len(), 1);
        assert_eq!(
            request.input.prompt_cache.breakpoints[0].position,
            lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 0 }
        );
        assert_eq!(
            request.execution.message_json_string_overrides["/messages/0/content/0/text"],
            vec![0xd800]
        );
        assert!(!request
            .execution
            .message_json_string_overrides
            .contains_key("/messages/0/content/1/text"));
    }
}
