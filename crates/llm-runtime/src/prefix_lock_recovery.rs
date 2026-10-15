//! Source-preserving repair for Native Anthropic strict prefix-lock errors.
//!
//! This module only plans a repair when the typed response coordinate, the
//! prepared request body, the canonical request, and the Host source sidecar
//! all identify one source thinking block. Missing or ambiguous provenance
//! falls back to the Native all-thinking repair.

use std::collections::{HashMap, HashSet};

use lingxi_core::types::{
    ContentBlock as HostBlock, ConversationMessage as HostMessage, MessageId,
};
use lingxi_llm_client::protocol::{
    ChatRequest, ContentBlock as WireBlock, JsonPayload, MessageRole, NativeAnthropicErrorFacts,
    NativeThinkingPrefixBlock, NativeThinkingPrefixMismatch,
};

/// Exact provider-facing identity used by Native's thinking-block key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThinkingPrefixBlockKey {
    Thinking(String),
    RedactedThinking(String),
}

impl ThinkingPrefixBlockKey {
    fn from_wire(block: &WireBlock) -> Option<Self> {
        match block {
            WireBlock::Thinking {
                signature: Some(signature),
                ..
            } if !signature.is_empty() => Some(Self::Thinking(signature.clone())),
            WireBlock::RedactedThinking { data } if !data.is_empty() => {
                Some(Self::RedactedThinking(data.clone()))
            }
            _ => None,
        }
    }

    fn from_host(block: &HostBlock) -> Option<Self> {
        match block {
            HostBlock::Thinking {
                signature: Some(signature),
                ..
            } if !signature.is_empty() => Some(Self::Thinking(signature.clone())),
            HostBlock::RedactedThinking { data } if !data.is_empty() => {
                Some(Self::RedactedThinking(data.clone()))
            }
            _ => None,
        }
    }

    fn from_json(body: &JsonPayload, message_index: usize, block_index: usize) -> Option<Self> {
        let pointer = format!("/messages/{message_index}/content/{block_index}");
        match body.string_at(&format!("{pointer}/type"))?.as_str() {
            "thinking" => body
                .string_at(&format!("{pointer}/signature"))
                .filter(|signature| !signature.is_empty())
                .map(Self::Thinking),
            "redacted_thinking" => body
                .string_at(&format!("{pointer}/data"))
                .filter(|data| !data.is_empty())
                .map(Self::RedactedThinking),
            _ => None,
        }
    }
}

/// One assistant message from the source history before request projection.
/// None keeps the thinking ordinal while indicating that Native cannot key
/// this unsigned block for a targeted repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceThinkingMessage {
    pub message_id: MessageId,
    pub blocks: Vec<Option<ThinkingPrefixBlockKey>>,
}

/// Capture source identities before normalization removes history rows or
/// content. A later request-local prefix marker truncates these rows at its
/// current range so duplicate signatures in already-stripped suffixes do not
/// make a future Native target look ambiguous.
pub(crate) fn capture_source_thinking_messages(
    messages: &[HostMessage],
) -> Vec<SourceThinkingMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let HostMessage::Assistant { id, content, .. } = message else {
                return None;
            };
            let blocks = content
                .iter()
                .filter(|block| {
                    matches!(
                        block,
                        HostBlock::Thinking { .. } | HostBlock::RedactedThinking { .. }
                    )
                })
                .map(ThinkingPrefixBlockKey::from_host)
                .collect::<Vec<_>>();
            (!blocks.is_empty()).then_some(SourceThinkingMessage {
                message_id: *id,
                blocks,
            })
        })
        .collect()
}

/// Current request coordinate and durable source range for one unique target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixLockTarget {
    pub request_message_index: usize,
    pub request_block_index: usize,
    pub source_message_id: MessageId,
    pub source_thinking_index: usize,
}

/// Action selected for one matching Native 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixLockPlan {
    /// The first two strict failures may strip from a unique targeted block.
    Partial(PrefixLockTarget),
    /// Native's untargeted or later-attempt fallback.
    All,
    /// Native's resume guard suppresses mutation; the decline belongs only to
    /// this exact error and is never latched across requests.
    Declined { mark_same_error: bool },
}

/// Per-logical-model-call state corresponding to Native's match count and
/// already-stripped guard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PrefixLockRecoveryState {
    matching_errors: u32,
    stripped_this_call: bool,
}

impl PrefixLockRecoveryState {
    pub(crate) fn record_matching_error(&mut self) -> u32 {
        self.matching_errors = self.matching_errors.saturating_add(1);
        self.matching_errors
    }

    pub(crate) fn mark_stripped(&mut self) {
        self.stripped_this_call = true;
    }

    pub(crate) fn stripped_this_call(self) -> bool {
        self.stripped_this_call
    }
}

/// Whether this error has Native's HTTP-400 strict-prefix message evidence.
/// The SDK sets these facts only on the Native Anthropic route; the explicit
/// provider/protocol checks keep that boundary visible to the Host.
pub(crate) fn is_native_prefix_lock_400(status: u16, facts: &NativeAnthropicErrorFacts) -> bool {
    status == 400
        && facts.provider_id.as_str() == "anthropic"
        && facts.protocol == lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages
        && facts
            .native_hq_hrn15_evidence
            .as_ref()
            .is_some_and(|evidence| evidence.prefix_lock_message_match)
}

/// Select the targeted or all-thinking repair for an already-classified
/// Native prefix-lock error. source_block_ids is projected through
/// history_input; every row must align exactly with the canonical request.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_prefix_lock_recovery(
    status: u16,
    facts: &NativeAnthropicErrorFacts,
    mismatch: Option<&NativeThinkingPrefixMismatch>,
    request: &ChatRequest,
    prepared_body: &JsonPayload,
    source_block_ids: &[Vec<HashSet<MessageId>>],
    source_thinking_messages: &[SourceThinkingMessage],
    thinking_source_message_ids: &[MessageId],
    prior_markers: &HashMap<MessageId, usize>,
    attempt: u32,
    stripped_this_call: bool,
    resume_incomplete_thinking: bool,
) -> Option<PrefixLockPlan> {
    if !is_native_prefix_lock_400(status, facts) {
        return None;
    }

    let mark_same_error = !stripped_this_call;
    if resume_incomplete_thinking {
        return Some(PrefixLockPlan::Declined { mark_same_error });
    }

    if attempt <= 2 {
        if let Some(target) = resolve_unique_target(
            mismatch.and_then(|mismatch| mismatch.block.as_ref()),
            request,
            prepared_body,
            source_block_ids,
            source_thinking_messages,
            thinking_source_message_ids,
            prior_markers,
        ) {
            return Some(PrefixLockPlan::Partial(target));
        }
    }
    Some(PrefixLockPlan::All)
}

fn resolve_unique_target(
    coordinate: Option<&NativeThinkingPrefixBlock>,
    request: &ChatRequest,
    prepared_body: &JsonPayload,
    source_block_ids: &[Vec<HashSet<MessageId>>],
    source_thinking_messages: &[SourceThinkingMessage],
    thinking_source_message_ids: &[MessageId],
    prior_markers: &HashMap<MessageId, usize>,
) -> Option<PrefixLockTarget> {
    let coordinate = coordinate?;
    let message_index = usize::try_from(coordinate.message_index).ok()?;
    let block_index = usize::try_from(coordinate.block_index).ok()?;
    let key = ThinkingPrefixBlockKey::from_json(prepared_body, message_index, block_index)?;
    if count_body_key(prepared_body, &key) != 1
        || prepared_body
            .string_at(&format!("/messages/{message_index}/role"))
            .as_deref()
            != Some("assistant")
    {
        return None;
    }

    if request.messages.len() != source_block_ids.len()
        || request
            .messages
            .iter()
            .zip(source_block_ids)
            .any(|(message, sources)| message.content.len() != sources.len())
    {
        return None;
    }

    // The prepared body coordinate must point at the same keyed block in the
    // canonical request, and the key must occur only once in that request.
    let mut request_match = None;
    let mut request_matches = 0usize;
    for (request_message_index, message) in request.messages.iter().enumerate() {
        if message.role != MessageRole::Assistant {
            continue;
        }
        for (request_block_index, block) in message.content.iter().enumerate() {
            if ThinkingPrefixBlockKey::from_wire(block).as_ref() == Some(&key) {
                request_matches += 1;
                request_match = Some((request_message_index, request_block_index));
            }
        }
    }
    if request_matches != 1 {
        return None;
    }
    let (request_message_index, request_block_index) = request_match?;
    let source_ids = &source_block_ids[request_message_index][request_block_index];
    if source_ids.len() != 1 {
        return None;
    }
    let source_message_id = *source_ids.iter().next()?;
    if thinking_source_message_ids
        .iter()
        .filter(|id| **id == source_message_id)
        .count()
        != 1
    {
        return None;
    }

    let mut source_match = None;
    let mut source_matches = 0usize;
    for source_message in source_thinking_messages {
        let limit = prior_markers
            .get(&source_message.message_id)
            .copied()
            .unwrap_or(source_message.blocks.len())
            .min(source_message.blocks.len());
        for (thinking_index, block_key) in source_message.blocks.iter().take(limit).enumerate() {
            if block_key.as_ref() == Some(&key) {
                source_matches += 1;
                source_match = Some((source_message.message_id, thinking_index));
            }
        }
    }
    if source_matches != 1 {
        return None;
    }
    let (matched_source_id, source_thinking_index) = source_match?;
    if matched_source_id != source_message_id {
        return None;
    }
    Some(PrefixLockTarget {
        request_message_index,
        request_block_index,
        source_message_id,
        source_thinking_index,
    })
}

fn count_body_key(body: &JsonPayload, key: &ThinkingPrefixBlockKey) -> usize {
    let Some(message_count) = body.array_len("/messages") else {
        return 0;
    };
    let mut matches = 0;
    for message_index in 0..message_count {
        if body
            .string_at(&format!("/messages/{message_index}/role"))
            .as_deref()
            != Some("assistant")
        {
            continue;
        }
        let Some(block_count) = body.array_len(&format!("/messages/{message_index}/content"))
        else {
            continue;
        };
        for block_index in 0..block_count {
            if ThinkingPrefixBlockKey::from_json(body, message_index, block_index).as_ref()
                == Some(key)
            {
                matches += 1;
            }
        }
    }
    matches
}

/// Apply Native's partial OYe behavior to the request-local clone only.
/// Durable transcript content is recorded through ThinkingRecoveryScope.
pub(crate) fn strip_partial_request(
    request: &mut crate::LlmRequest,
    target: PrefixLockTarget,
) -> bool {
    let Some(target_message) = request.input.messages.get(target.request_message_index) else {
        return false;
    };
    let Some(target_block) = target_message.content.get(target.request_block_index) else {
        return false;
    };
    if target_message.role != MessageRole::Assistant
        || ThinkingPrefixBlockKey::from_wire(target_block).is_none()
    {
        return false;
    }

    let roles = request
        .input
        .messages
        .iter()
        .map(|message| message.role)
        .collect::<Vec<_>>();
    let mut block_indices = vec![0usize; roles.len()];
    crate::model::thinking_signature::retain_input_blocks(
        request,
        |message_index, block| {
            let Some(current_block_index) = block_indices.get_mut(message_index) else {
                return true;
            };
            let block_index = *current_block_index;
            *current_block_index += 1;
            if message_index < target.request_message_index
                || roles.get(message_index) != Some(&MessageRole::Assistant)
            {
                return true;
            }
            if message_index == target.request_message_index
                && block_index < target.request_block_index
            {
                return true;
            }
            if is_thinking(block) {
                return false;
            }
            !is_blank_text(block)
        },
        Some("[Thinking removed]"),
    )
}

pub(crate) fn strip_all_request_thinking(request: &mut crate::LlmRequest) -> bool {
    crate::model::thinking_signature::strip_input_thinking(request)
}

/// Convert the source-ordered range into the existing durable recorder format.
/// The target begins at its source thinking ordinal; later source rows strip
/// from zero. An absent/duplicate target identity is not a partial repair.
pub(crate) fn partial_source_ranges(
    target: PrefixLockTarget,
    thinking_source_message_ids: &[MessageId],
) -> Option<HashMap<MessageId, usize>> {
    let positions = thinking_source_message_ids
        .iter()
        .enumerate()
        .filter(|(_, id)| **id == target.source_message_id)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [target_position] = positions.as_slice() else {
        return None;
    };
    let mut ranges = HashMap::new();
    for (index, id) in thinking_source_message_ids
        .iter()
        .enumerate()
        .skip(*target_position)
    {
        let from = if index == *target_position {
            target.source_thinking_index
        } else {
            0
        };
        ranges
            .entry(*id)
            .and_modify(|earliest: &mut usize| *earliest = (*earliest).min(from))
            .or_insert(from);
    }
    Some(ranges)
}

pub(crate) fn all_source_ranges(
    thinking_source_message_ids: &[MessageId],
) -> HashMap<MessageId, usize> {
    thinking_source_message_ids
        .iter()
        .copied()
        .map(|id| (id, 0))
        .collect()
}

fn is_thinking(block: &WireBlock) -> bool {
    matches!(
        block,
        WireBlock::Thinking { .. } | WireBlock::RedactedThinking { .. }
    )
}

fn is_blank_text(block: &WireBlock) -> bool {
    matches!(
        block,
        WireBlock::Text { text, .. } | WireBlock::TextJsUtf16 { text, .. }
            if text.trim().is_empty()
    )
}

#[cfg(test)]
#[path = "prefix_lock_recovery_test.rs"]
mod tests;
