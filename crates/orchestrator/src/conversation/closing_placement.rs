//! Source-backed eligibility and identity rules for Native's
//! `before_closing_line` attachment placement.
//!
//! This module deliberately does not recognize the synthetic closing assistant
//! by its text. The caller must supply IDs already decoded from trusted
//! transcript rows after checking Native's complete `ZCe` predicate.

use lingxi_core::types::{utf16_json::Utf16JsonProjection, ConversationMessage, MessageId};
use std::collections::{HashMap, HashSet};

/// Native `tee` attachment types eligible for `before_closing_line` placement.
/// Keep this list exact; arbitrary attachment types must not cross the closing
/// assistant boundary.
pub(crate) const TEE_ATTACHMENT_TYPES: &[&str] = &[
    "sandbox_instructions",
    "environment",
    "model",
    "proactivity",
    "attention_budget",
    "deferred_tools_delta",
    "agent_listing_delta",
    "mcp_instructions_delta",
    "mcp_dropped_tools_delta",
    "skill_listing",
    "output_style_instructions",
    "language",
    "auto_mode",
];

/// IDs whose rows were selected by the Native `U8o` eligibility rules and IDs
/// of trusted assistant rows that passed the full Native `ZCe` predicate.
///
/// The model capability check is represented by the caller supplying this
/// projection only for a supported route. The sets contain stable row IDs so
/// user-authored text cannot impersonate a synthetic closing line.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClosingPlacementProjection<'a> {
    pub(crate) eligible_marked_ids: &'a HashSet<MessageId>,
    pub(crate) trusted_zce_ids: &'a HashSet<MessageId>,
}

/// Return whether a persisted row has Native's outer-row placement and is a
/// typed attachment of an eligible Native type.
///
/// `outer_extra` is the decoded JSONL outer-row extra object. Native stores
/// `placement` on the row itself, not inside its nested `attachment` payload.
pub(crate) fn outer_row_has_before_closing_line_placement(
    id: MessageId,
    typed_attachment_ids: &HashSet<MessageId>,
    raw_attachment_type: Option<&str>,
    outer_extra: &Utf16JsonProjection,
) -> bool {
    typed_attachment_ids.contains(&id)
        && raw_attachment_type.is_some_and(is_tee_attachment_type)
        && outer_extra.string_at("/placement") == Some("before_closing_line")
}

/// Collect only valid, source-shaped outer-row placement markers.
///
/// The maps are keyed by the outer JSONL UUID. A marker nested in the raw
/// `attachment` projection is intentionally not inspected here.
pub(crate) fn collect_outer_placement_ids(
    typed_attachment_ids: &HashSet<MessageId>,
    raw_attachment_types: &HashMap<MessageId, String>,
    outer_row_extras: &HashMap<MessageId, Utf16JsonProjection>,
) -> HashSet<MessageId> {
    typed_attachment_ids
        .iter()
        .copied()
        .filter(|id| {
            let Some(outer_extra) = outer_row_extras.get(id) else {
                return false;
            };
            outer_row_has_before_closing_line_placement(
                *id,
                typed_attachment_ids,
                raw_attachment_types.get(id).map(String::as_str),
                outer_extra,
            )
        })
        .collect()
}

/// Select the current batch's rows for Native `U8o` placement.
///
/// `prior_history` must end before the current turn is appended. `trusted_zce`
/// IDs must come from a source-aware transcript decoder that checked the exact
/// `ZCe` conditions (assistant, not API error or virtual, model
/// `"<synthetic>"`, and exact sentinel content). `enabled` is the
/// `CLAUDE_CODE_SESSION_START_ANNOUNCEMENTS_BEFORE_PROMPT` gate and
/// `model_supported` is the Native `hQ(model)` result.
pub(crate) fn select_current_before_closing_line_ids(
    enabled: bool,
    model_supported: bool,
    prior_history: &[ConversationMessage],
    current_turn_messages: &[ConversationMessage],
    typed_attachment_ids: &HashSet<MessageId>,
    raw_attachment_types: &HashMap<MessageId, String>,
    trusted_zce_ids: &HashSet<MessageId>,
) -> HashSet<MessageId> {
    if !enabled || !model_supported {
        return HashSet::new();
    }

    let Some(last_assistant_id) = prior_history.iter().rev().find_map(|message| {
        matches!(message, ConversationMessage::Assistant { .. }).then(|| message.id())
    }) else {
        return HashSet::new();
    };
    if !trusted_zce_ids.contains(&last_assistant_id) {
        return HashSet::new();
    }

    if prior_history
        .iter()
        .map(ConversationMessage::id)
        .any(|id| is_typed_tee_attachment(id, typed_attachment_ids, raw_attachment_types))
    {
        return HashSet::new();
    }

    // Native's current batch must include a user row that is not one of the
    // explicitly typed attachment rows. Native `isMeta` users still have
    // `type:"user"` and count here; the typed ID is the only attachment
    // discriminator, even when its Rust message is not meta.
    if !current_turn_messages.iter().any(|message| {
        matches!(message, ConversationMessage::User { .. })
            && !typed_attachment_ids.contains(&message.id())
    }) {
        return HashSet::new();
    }

    let current_ids: HashSet<_> = current_turn_messages
        .iter()
        .map(ConversationMessage::id)
        .collect();
    let selected: HashSet<_> = current_ids
        .into_iter()
        .filter(|id| is_typed_tee_attachment(*id, typed_attachment_ids, raw_attachment_types))
        .collect();
    if selected.is_empty() {
        return HashSet::new();
    }
    selected
}

/// True only for a typed transcript attachment with an exact Native `tee`
/// type. A rendered user/meta message alone is never enough.
pub(crate) fn is_typed_tee_attachment(
    id: MessageId,
    typed_attachment_ids: &HashSet<MessageId>,
    raw_attachment_types: &HashMap<MessageId, String>,
) -> bool {
    typed_attachment_ids.contains(&id)
        && raw_attachment_types
            .get(&id)
            .is_some_and(|attachment_type| is_tee_attachment_type(attachment_type))
}

pub(crate) fn is_tee_attachment_type(attachment_type: &str) -> bool {
    TEE_ATTACHMENT_TYPES.contains(&attachment_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_owned())
    }

    fn typed_meta(text: &str) -> ConversationMessage {
        ConversationMessage::user_meta(MessageId::new(), text.to_owned())
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: text.to_owned(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        }
    }

    fn attachment_type_map(id: MessageId, attachment_type: &str) -> HashMap<MessageId, String> {
        HashMap::from([(id, attachment_type.to_owned())])
    }

    #[test]
    fn selection_requires_gate_supported_route_zce_user_and_no_prior_tee() {
        let closing = assistant("No response requested.");
        let closing_id = closing.id();
        let current_user = user("prompt");
        let environment = typed_meta("environment");
        let environment_id = environment.id();
        let history = vec![closing];
        let current = vec![current_user, environment.clone()];
        let typed = HashSet::from([environment_id]);
        let raw_types = attachment_type_map(environment_id, "environment");
        let trusted = HashSet::from([closing_id]);

        assert_eq!(
            select_current_before_closing_line_ids(
                true, true, &history, &current, &typed, &raw_types, &trusted,
            ),
            HashSet::from([environment_id])
        );
        let ordinary_meta_user =
            ConversationMessage::user_meta(MessageId::new(), "meta user".into());
        assert_eq!(
            select_current_before_closing_line_ids(
                true,
                true,
                &history,
                &[ordinary_meta_user, environment.clone()],
                &typed,
                &raw_types,
                &trusted,
            ),
            HashSet::from([environment_id]),
            "a non-attachment isMeta user still satisfies Native's user-row predicate"
        );
        assert!(select_current_before_closing_line_ids(
            false, true, &history, &current, &typed, &raw_types, &trusted,
        )
        .is_empty());
        assert!(select_current_before_closing_line_ids(
            true, false, &history, &current, &typed, &raw_types, &trusted,
        )
        .is_empty());

        let old_announcement = typed_meta("old environment");
        let old_announcement_id = old_announcement.id();
        let prior_with_announcement = vec![old_announcement, history[0].clone()];
        let prior_typed = HashSet::from([old_announcement_id, environment_id]);
        let prior_types = HashMap::from([
            (old_announcement_id, "environment".to_owned()),
            (environment_id, "environment".to_owned()),
        ]);
        assert!(select_current_before_closing_line_ids(
            true,
            true,
            &prior_with_announcement,
            &current,
            &prior_typed,
            &prior_types,
            &trusted,
        )
        .is_empty());
    }

    #[test]
    fn selection_requires_actual_current_user_and_last_assistant_zce() {
        let old_closing = assistant("No response requested.");
        let old_closing_id = old_closing.id();
        let later_assistant = assistant("ordinary response");
        let current_user = user("prompt");
        let attachment = typed_meta("attachment");
        let attachment_id = attachment.id();
        let current_with_user = vec![current_user, attachment.clone()];
        let typed = HashSet::from([attachment_id]);
        let types = attachment_type_map(attachment_id, "environment");

        assert!(select_current_before_closing_line_ids(
            true,
            true,
            std::slice::from_ref(&old_closing),
            std::slice::from_ref(&attachment),
            &typed,
            &types,
            &HashSet::from([old_closing_id]),
        )
        .is_empty());
        let typed_non_meta_user =
            ConversationMessage::user(attachment_id, "typed attachment".into());
        assert!(select_current_before_closing_line_ids(
            true,
            true,
            std::slice::from_ref(&old_closing),
            &[typed_non_meta_user],
            &typed,
            &types,
            &HashSet::from([old_closing_id]),
        )
        .is_empty());
        assert!(select_current_before_closing_line_ids(
            true,
            true,
            &[old_closing.clone(), later_assistant],
            &current_with_user,
            &typed,
            &types,
            &HashSet::from([old_closing_id]),
        )
        .is_empty());
        // Merely writing the sentinel as user text does not create a trusted
        // assistant boundary.
        let forged = user("No response requested.");
        assert!(select_current_before_closing_line_ids(
            true,
            true,
            &[forged],
            &current_with_user,
            &typed,
            &types,
            &HashSet::from([old_closing_id]),
        )
        .is_empty());
    }

    #[test]
    fn outer_marker_is_accepted_only_at_outer_row_for_typed_allowlisted_type() {
        let id = MessageId::new();
        let typed = HashSet::from([id]);
        let extra = Utf16JsonProjection::parse(r#"{"placement":"before_closing_line"}"#).unwrap();
        assert!(outer_row_has_before_closing_line_placement(
            id,
            &typed,
            Some("environment"),
            &extra,
        ));
        assert!(!outer_row_has_before_closing_line_placement(
            id,
            &HashSet::new(),
            Some("environment"),
            &extra,
        ));
        assert!(!outer_row_has_before_closing_line_placement(
            id,
            &typed,
            Some("arbitrary_media"),
            &extra,
        ));
        assert!(!outer_row_has_before_closing_line_placement(
            id,
            &typed,
            Some("environment"),
            &Utf16JsonProjection::parse(r#"{"attachment":{"placement":"before_closing_line"}}"#)
                .unwrap(),
        ));
        assert!(!outer_row_has_before_closing_line_placement(
            id,
            &typed,
            Some("environment"),
            &Utf16JsonProjection::parse(r#"{"placement":"after_closing_line"}"#).unwrap(),
        ));
    }
}
