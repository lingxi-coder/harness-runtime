//! Repack typed attachment rows around their Native request boundaries.
//!
//! Native's `zjr` scans the normalized history backward, buffering attachment
//! rows until it reaches an assistant message or a user tool-result/task-delivery
//! boundary. Ordinary user and meta-user messages do not flush that buffer.
//! Only prevalidated `before_closing_line` attachment IDs may remain buffered
//! across a trusted Native synthetic closing assistant; every other assistant
//! boundary flushes as usual. The final reverse preserves row identity.
//!
//! The caller supplies attachment identities from typed transcript metadata.
//! Generic `is_meta` rows are deliberately not classified as attachments.

use super::closing_placement::ClosingPlacementProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use std::collections::HashSet;

/// Repack typed attachment rows with Native's backward-scan ordering rule.
///
/// `typed_attachment_ids` must contain only IDs whose transcript attachment
/// payload is known to the caller. `task_delivery_boundary_ids` is derived from
/// current request-history metadata, where the Native `taskDelivery` source is
/// stored beside each user message rather than inline in the
/// `ConversationMessage` carrier. Those IDs mark user boundaries. A typed
/// attachment ID takes precedence if it is also present in that boundary set.
#[cfg(test)]
pub(crate) fn repack_typed_attachments(
    messages: &mut Vec<ConversationMessage>,
    typed_attachment_ids: &HashSet<MessageId>,
    task_delivery_boundary_ids: Option<&HashSet<MessageId>>,
    closing_placement: Option<ClosingPlacementProjection<'_>>,
) {
    repack_typed_attachments_with_trailing_ids(
        messages,
        typed_attachment_ids,
        task_delivery_boundary_ids,
        closing_placement,
        &HashSet::new(),
    );
}

/// Repack selected current-batch attachments after the other typed rows in
/// their existing Native boundary group. The input buffer is reverse-scanned,
/// so selected IDs are sorted to its front and land at the group's tail after
/// the final reverse. Stable sorting preserves order within both classes.
pub(crate) fn repack_typed_attachments_with_trailing_ids(
    messages: &mut Vec<ConversationMessage>,
    typed_attachment_ids: &HashSet<MessageId>,
    task_delivery_boundary_ids: Option<&HashSet<MessageId>>,
    closing_placement: Option<ClosingPlacementProjection<'_>>,
    trailing_typed_attachment_ids: &HashSet<MessageId>,
) {
    if !messages
        .iter()
        .any(|message| typed_attachment_ids.contains(&message.id()))
    {
        return;
    }

    let mut reversed = Vec::with_capacity(messages.len());
    let mut pending_attachments = Vec::new();

    for message in messages.drain(..).rev() {
        let id = message.id();
        if typed_attachment_ids.contains(&id) {
            pending_attachments.push(message);
            continue;
        }

        if is_boundary(&message, task_delivery_boundary_ids) && !pending_attachments.is_empty() {
            sort_selected_first_in_reverse_scan(
                &mut pending_attachments,
                trailing_typed_attachment_ids,
            );
            // The source appends the backward-ordered attachment buffer before
            // the boundary, then reverses the completed output once at the end.
            if let (
                ConversationMessage::Assistant { .. },
                Some(ClosingPlacementProjection {
                    eligible_marked_ids,
                    trusted_zce_ids,
                }),
            ) = (&message, closing_placement)
            {
                if trusted_zce_ids.contains(&id) {
                    let mut ordinary = Vec::new();
                    let mut retained = Vec::new();
                    for attachment in pending_attachments.drain(..) {
                        let attachment_id = attachment.id();
                        if typed_attachment_ids.contains(&attachment_id)
                            && eligible_marked_ids.contains(&attachment_id)
                        {
                            retained.push(attachment);
                        } else {
                            ordinary.push(attachment);
                        }
                    }
                    reversed.append(&mut ordinary);
                    pending_attachments = retained;
                } else {
                    reversed.append(&mut pending_attachments);
                }
            } else {
                reversed.append(&mut pending_attachments);
            }
            reversed.push(message);
        } else {
            reversed.push(message);
        }
    }

    sort_selected_first_in_reverse_scan(&mut pending_attachments, trailing_typed_attachment_ids);
    reversed.append(&mut pending_attachments);
    reversed.reverse();
    *messages = reversed;
}

fn sort_selected_first_in_reverse_scan(
    pending_attachments: &mut [ConversationMessage],
    trailing_typed_attachment_ids: &HashSet<MessageId>,
) {
    if pending_attachments
        .iter()
        .any(|attachment| trailing_typed_attachment_ids.contains(&attachment.id()))
    {
        pending_attachments
            .sort_by_key(|attachment| !trailing_typed_attachment_ids.contains(&attachment.id()));
    }
}

fn is_boundary(
    message: &ConversationMessage,
    task_delivery_boundary_ids: Option<&HashSet<MessageId>>,
) -> bool {
    match message {
        ConversationMessage::Assistant { .. } => true,
        ConversationMessage::User { id, content, .. } => {
            matches!(content.first(), Some(ContentBlock::ToolResult { .. }))
                || task_delivery_boundary_ids.is_some_and(|ids| ids.contains(id))
        }
        ConversationMessage::System { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{utf16_json::Utf16JsonProjection, ToolUseId};

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_owned())
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        }
    }

    fn typed_meta(text: &str) -> ConversationMessage {
        ConversationMessage::user_meta(MessageId::new(), text.to_owned())
    }

    fn ids(messages: &[ConversationMessage]) -> Vec<MessageId> {
        messages.iter().map(ConversationMessage::id).collect()
    }

    fn fallback_with_source_units(id: MessageId) -> (ConversationMessage, Vec<u16>) {
        let mut units = "<system-reminder>unknown slash "
            .encode_utf16()
            .collect::<Vec<_>>();
        units.push(0xd800);
        units.extend(" fallback</system-reminder>".encode_utf16());
        let text = String::from_utf16_lossy(&units);
        let mut message = ConversationMessage::user(id, text.clone());
        if let ConversationMessage::User { content, .. } = &mut message {
            *content = vec![ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units: units.clone(),
                citations: None,
            }];
        }
        (message, units)
    }

    fn tool_result_user() -> ConversationMessage {
        let mut message = ConversationMessage::user(MessageId::new(), String::new());
        if let ConversationMessage::User { content, .. } = &mut message {
            content[0] = ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: Utf16JsonProjection::parse(r#""tool result""#).unwrap(),
                is_error: None,
                provider_tool_use_id: None,
                content_blocks: None,
            };
        }
        message
    }

    #[test]
    fn historical_and_current_attachments_repack_before_each_ordinary_user() {
        let first_user = user("first prompt");
        let generic_meta = ConversationMessage::user_meta(
            MessageId::new(),
            "generic meta is not a typed attachment".into(),
        );
        let environment = typed_meta("environment");
        let first_assistant = assistant("first answer");
        let second_user = user("second prompt");
        let fallback_id = MessageId::new();
        let (fallback, source_units) = fallback_with_source_units(fallback_id);
        let second_assistant = assistant("second answer");
        let third_user = user("third prompt");

        let environment_id = environment.id();
        let mut typed_attachment_ids = HashSet::new();
        typed_attachment_ids.insert(environment_id);
        typed_attachment_ids.insert(fallback_id);
        let mut messages = vec![
            first_user.clone(),
            generic_meta.clone(),
            environment.clone(),
            first_assistant.clone(),
            second_user.clone(),
            fallback.clone(),
            second_assistant.clone(),
            third_user.clone(),
        ];

        repack_typed_attachments(&mut messages, &typed_attachment_ids, None, None);

        let expected = vec![
            environment,
            first_user,
            generic_meta,
            first_assistant,
            fallback.clone(),
            second_user,
            second_assistant,
            third_user,
        ];
        assert_eq!(messages, expected, "row identity and payload stay intact");
        assert_eq!(
            ids(&messages),
            ids(&expected),
            "the multi-turn projection keeps every message exactly once"
        );
        assert!(matches!(
            &messages[4],
            ConversationMessage::User { content, .. }
                if matches!(content.first(), Some(ContentBlock::TextJsUtf16 { utf16_code_units, .. }) if utf16_code_units == &source_units)
        ));
        assert!(
            !messages[4].is_meta(),
            "the fallback's typed status comes from its ID"
        );
    }

    #[test]
    fn first_tool_result_user_is_an_attachment_flush_boundary() {
        let before = user("before result");
        let result = tool_result_user();
        let attachment = typed_meta("after tool result");
        let after = user("after result");
        let attachment_id = attachment.id();
        let mut messages = vec![
            before.clone(),
            result.clone(),
            attachment.clone(),
            after.clone(),
        ];

        repack_typed_attachments(&mut messages, &HashSet::from([attachment_id]), None, None);

        assert_eq!(messages, vec![before, result, attachment, after]);
    }

    #[test]
    fn only_current_upe_ids_trail_nar_within_their_existing_group() {
        let prior_user = user("prior command");
        let prior_upe_success = typed_meta("finished UPE success");
        let prior_nar = typed_meta("finished NAr row");
        let prior_assistant = assistant("finished turn");
        let current_user = user("current command");
        let current_upe_success = typed_meta("current UPE success");
        let current_nar = typed_meta("current NAr row");
        let current_upe_context = typed_meta("current UPE context");
        let current_body = user("current expanded body");
        let typed_ids = HashSet::from([
            prior_upe_success.id(),
            prior_nar.id(),
            current_upe_success.id(),
            current_nar.id(),
            current_upe_context.id(),
        ]);
        let current_upe_ids = HashSet::from([current_upe_success.id(), current_upe_context.id()]);
        let mut messages = vec![
            prior_user.clone(),
            prior_upe_success.clone(),
            prior_nar.clone(),
            prior_assistant.clone(),
            current_user.clone(),
            current_upe_success.clone(),
            current_nar.clone(),
            current_upe_context.clone(),
            current_body.clone(),
        ];

        repack_typed_attachments_with_trailing_ids(
            &mut messages,
            &typed_ids,
            None,
            None,
            &current_upe_ids,
        );

        let expected = vec![
            prior_upe_success,
            prior_nar,
            prior_user,
            prior_assistant,
            current_nar,
            current_upe_success,
            current_upe_context,
            current_user,
            current_body,
        ];
        assert_eq!(messages, expected);
        let first_projection = messages.clone();
        repack_typed_attachments_with_trailing_ids(
            &mut messages,
            &typed_ids,
            None,
            None,
            &current_upe_ids,
        );
        assert_eq!(messages, first_projection, "projection remains idempotent");
    }

    #[test]
    fn task_delivery_boundary_is_explicit() {
        let before = user("before delivery");
        let delivery = ConversationMessage::user_meta(MessageId::new(), "task delivery".into());
        let attachment = typed_meta("typed row");
        let after = user("after delivery");
        let delivery_id = delivery.id();
        let attachment_id = attachment.id();
        let typed_ids = HashSet::from([attachment_id]);
        let mut messages = vec![
            before.clone(),
            delivery.clone(),
            attachment.clone(),
            after.clone(),
        ];

        let no_task_boundary = {
            let mut messages = messages.clone();
            repack_typed_attachments(&mut messages, &typed_ids, None, None);
            messages
        };
        repack_typed_attachments(
            &mut messages,
            &typed_ids,
            Some(&HashSet::from([delivery_id])),
            None,
        );

        assert_eq!(messages, vec![before, delivery, attachment, after]);
        assert_ne!(
            messages, no_task_boundary,
            "the supplied delivery ID is a boundary"
        );
    }

    #[test]
    fn typed_attachment_identity_takes_precedence_over_boundary_ids() {
        let before = user("before typed rows");
        let first = typed_meta("first typed row");
        let second = typed_meta("second typed row");
        let after = user("after typed rows");
        let typed_ids = HashSet::from([first.id(), second.id()]);
        let boundary_ids = HashSet::from([second.id()]);
        let mut messages = vec![before.clone(), first.clone(), second.clone(), after.clone()];

        repack_typed_attachments(&mut messages, &typed_ids, Some(&boundary_ids), None);

        assert_eq!(messages, vec![first, second, before, after]);
    }

    #[test]
    fn generic_meta_is_not_a_boundary_or_a_typed_attachment() {
        let first = user("first");
        let generic_meta = ConversationMessage::user_meta(MessageId::new(), "meta".into());
        let attachment = typed_meta("typed");
        let assistant = assistant("boundary");
        let attachment_id = attachment.id();
        let mut messages = vec![
            first.clone(),
            generic_meta.clone(),
            attachment.clone(),
            assistant.clone(),
        ];

        repack_typed_attachments(&mut messages, &HashSet::from([attachment_id]), None, None);

        assert_eq!(messages, vec![attachment, first, generic_meta, assistant]);
    }

    #[test]
    fn projection_is_idempotent_for_already_repacked_history() {
        let first = user("first");
        let old_attachment = typed_meta("old attachment");
        let assistant = assistant("answer");
        let second = user("second");
        let new_attachment = typed_meta("new attachment");
        let ids = HashSet::from([old_attachment.id(), new_attachment.id()]);
        let mut messages = vec![first, old_attachment, assistant, second, new_attachment];

        repack_typed_attachments(&mut messages, &ids, None, None);
        let first_projection = messages.clone();
        repack_typed_attachments(&mut messages, &ids, None, None);

        assert_eq!(messages, first_projection);
    }

    #[test]
    fn marked_tee_attachment_crosses_only_trusted_zce_boundary() {
        use super::super::closing_placement::ClosingPlacementProjection;

        let prior_assistant = assistant("prior answer");
        let closing_assistant = assistant("No response requested.");
        let current_user = user("current prompt");
        let marked_id = MessageId::new();
        let (marked, source_units) = fallback_with_source_units(marked_id);
        let ordinary_attachment = typed_meta("ordinary attachment");
        let ordinary_id = ordinary_attachment.id();
        let typed_ids = HashSet::from([marked_id, ordinary_id]);
        let eligible_marked_ids = HashSet::from([marked_id]);
        let trusted_zce_ids = HashSet::from([closing_assistant.id()]);
        let mut messages = vec![
            prior_assistant.clone(),
            closing_assistant.clone(),
            current_user.clone(),
            marked.clone(),
            ordinary_attachment.clone(),
        ];

        repack_typed_attachments(
            &mut messages,
            &typed_ids,
            None,
            Some(ClosingPlacementProjection {
                eligible_marked_ids: &eligible_marked_ids,
                trusted_zce_ids: &trusted_zce_ids,
            }),
        );

        assert_eq!(
            messages,
            vec![
                prior_assistant,
                marked.clone(),
                closing_assistant,
                ordinary_attachment,
                current_user,
            ],
            "only the marked eligible row moves before the closing line"
        );
        assert!(matches!(
            &messages[1],
            ConversationMessage::User { content, .. }
                if matches!(content.first(), Some(ContentBlock::TextJsUtf16 { utf16_code_units, .. }) if utf16_code_units == &source_units)
        ));
    }

    #[test]
    fn marked_attachment_does_not_cross_untrusted_or_ordinary_assistant() {
        use super::super::closing_placement::ClosingPlacementProjection;

        let prior_assistant = assistant("prior answer");
        let ordinary_assistant = assistant("ordinary answer");
        let current_user = user("current prompt");
        let marked = typed_meta("marked attachment");
        let marked_id = marked.id();
        let typed_ids = HashSet::from([marked_id]);
        let eligible_marked_ids = HashSet::from([marked_id]);
        // This identifies an older synthetic closing row, not the last
        // assistant boundary being visited by this projection.
        let trusted_zce_ids = HashSet::from([MessageId::new()]);
        let mut messages = vec![
            prior_assistant.clone(),
            ordinary_assistant.clone(),
            current_user.clone(),
            marked.clone(),
        ];

        repack_typed_attachments(
            &mut messages,
            &typed_ids,
            None,
            Some(ClosingPlacementProjection {
                eligible_marked_ids: &eligible_marked_ids,
                trusted_zce_ids: &trusted_zce_ids,
            }),
        );

        assert_eq!(
            messages,
            vec![prior_assistant, ordinary_assistant, marked, current_user],
            "an ordinary assistant still flushes the attachment buffer"
        );
    }
}
