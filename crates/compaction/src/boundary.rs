//! Compact-boundary message: the typed transition marker the session loader
//! and `get_messages_after_compact_boundary` slice on.
//!
//! TS ref: `src/utils/messages.ts:4530-4555` (`createCompactBoundaryMessage`),
//! `:4608-4656` (`isCompactBoundaryMessage`, `findLastCompactBoundaryIndex`,
//! `getMessagesAfterCompactBoundary`); `src/services/compact/compact.ts:349-368`
//! (`annotateBoundaryWithPreservedSegment`), `:600-611` / `:330-338`
//! (`preCompactDiscoveredTools`, `buildPostCompactMessages`).
//!
//! The in-memory marker now carries the same typed `subtype` and
//! `compactMetadata` fields as the persisted boundary. Recognition requires
//! the current typed subtype, matching native 2.1.287 Bi; ordinary system text
//! cannot truncate model history.

pub use lingxi_core::types::{
    CompactActiveGoalState, CompactBoundaryMetadata, CompactGoalOrigin, CompactTrigger,
    PreservedMessages, PreservedSegment,
};
use lingxi_core::types::{ConversationMessage, MessageId};

/// Exact content string TS stamps on every compact-boundary system message
/// (`createCompactBoundaryMessage` sets `content` to `Conversation compacted`).
pub const BOUNDARY_CONTENT: &str = "Conversation compacted";

/// Convert the engine-owned active-goal state into its compact wire snapshot.
///
/// Keeping this conversion explicit prevents arbitrary JSON from entering the
/// typed compact metadata while avoiding a protocol→engine dependency cycle.
#[must_use]
pub fn compact_active_goal_from_engine(
    goal: &lingxi_core::session::ActiveGoalState,
) -> CompactActiveGoalState {
    CompactActiveGoalState {
        condition: goal.condition.clone(),
        set_at: goal.set_at,
        last_reason: goal.last_reason.clone(),
        iterations: goal.iterations,
        tokens_at_start: goal.tokens_at_start,
        origin: match goal.origin {
            lingxi_core::session::GoalOrigin::User => CompactGoalOrigin::User,
            lingxi_core::session::GoalOrigin::Restored => CompactGoalOrigin::Restored,
        },
    }
}

/// Convert a compact goal snapshot back into the engine session type.
#[must_use]
pub fn compact_active_goal_into_engine(
    goal: CompactActiveGoalState,
) -> lingxi_core::session::ActiveGoalState {
    lingxi_core::session::ActiveGoalState {
        condition: goal.condition,
        set_at: goal.set_at,
        last_reason: goal.last_reason,
        iterations: goal.iterations,
        tokens_at_start: goal.tokens_at_start,
        origin: match goal.origin {
            CompactGoalOrigin::User => lingxi_core::session::GoalOrigin::User,
            CompactGoalOrigin::Restored => lingxi_core::session::GoalOrigin::Restored,
        },
    }
}

/// Construct a compact-boundary system message plus its typed metadata.
///
/// 1:1 with `createCompactBoundaryMessage` (TS `messages.ts:4530`):
///  - `content` is the exact `"Conversation compacted"` sentinel,
///  - `trigger` / `pre_tokens` / `user_context` / `messages_summarized` map to
///    `compactMetadata`,
///  - `last_pre_compact_message_uuid` → `logicalParentUuid` (only when present),
///  - `discovered_tools` are sorted and deduped to mirror the TS
///    `[...preCompactDiscovered].sort()` carry (`compact.ts:608`).
///
/// Returns the in-history [`ConversationMessage`] marker and the rich
/// [`CompactBoundaryMetadata`].
#[must_use]
pub fn create_compact_boundary(
    trigger: CompactTrigger,
    pre_tokens: u64,
    last_pre_compact_message_uuid: Option<MessageId>,
    user_context: Option<String>,
    messages_summarized: Option<u32>,
    discovered_tools: &[String],
) -> (ConversationMessage, CompactBoundaryMetadata) {
    // Mirror `[...preCompactDiscovered].sort()`: sort + dedup. The TS set is
    // already unique; sorting a deduped vec yields the identical array.
    let mut tools: Vec<String> = discovered_tools.to_vec();
    tools.sort();
    tools.dedup();

    let metadata = CompactBoundaryMetadata {
        trigger,
        pre_tokens,
        post_tokens: None,
        cumulative_dropped_tokens: None,
        duration_ms: None,
        precomputed: None,
        user_context,
        messages_summarized,
        pre_compact_discovered_tools: tools,
        preserved_segment: None,
        preserved_messages: None,
        active_goal: None,
        // Bare 8-4-4-4-12 uuid (NOT the `msg:`-prefixed Display form) — this
        // value must match on-disk JSONL line uuids, which are raw.
        logical_parent_uuid: last_pre_compact_message_uuid.map(|u| u.as_uuid().to_string()),
    };

    let marker = ConversationMessage::compact_boundary(
        MessageId::new(),
        BOUNDARY_CONTENT.to_string(),
        metadata.clone(),
    );

    (marker, metadata)
}

/// Build a [`PreservedSegment`] from a kept tail of messages and its anchor.
///
/// 1:1 with the `preservedSegment` object `WAo` stamps onto the boundary
/// (`bin/claude.exe` offset 202984952):
/// `{headUuid: keep[0].uuid, anchorUuid: <anchor>, tailUuid: keep.at(-1).uuid}`.
/// The `anchor` is the message the loader splices the preserved tail AFTER — the
/// last summary message for suffix-preserving compaction (so the kept tail
/// follows the summary), or the boundary marker for prefix-preserving. Returns
/// `None` when the kept tail is empty (no relink needed), matching the binary's
/// `s.length>0 && {preservedSegment:…}` guard.
#[must_use]
pub fn preserved_segment_for_tail(
    kept_tail: &[ConversationMessage],
    anchor_uuid: Option<&MessageId>,
) -> Option<PreservedSegment> {
    let head = kept_tail.first()?;
    let tail = kept_tail.last()?;
    Some(PreservedSegment {
        head_uuid: Some(message_uuid(head)),
        anchor_uuid: anchor_uuid.map(|u| u.as_uuid().to_string()),
        tail_uuid: Some(message_uuid(tail)),
    })
}

/// Build a [`PreservedMessages`] re-splice list from a kept tail + anchor.
///
/// TS stamps `compactMetadata.preservedMessages = {anchorUuid, uuids,
/// allUuids}` alongside `preservedSegment`; the loader (`E$_`) re-parents
/// `uuids` onto `anchorUuid` in sequence at cold load. This port keeps only
/// chain participants in memory, so `uuids == allUuids`. Returns `None` for an
/// empty tail, mirroring [`preserved_segment_for_tail`].
#[must_use]
pub fn preserved_messages_for_tail(
    kept_tail: &[ConversationMessage],
    anchor_uuid: Option<&MessageId>,
) -> Option<PreservedMessages> {
    if kept_tail.is_empty() {
        return None;
    }
    let uuids: Vec<String> = kept_tail.iter().map(message_uuid).collect();
    Some(PreservedMessages {
        anchor_uuid: anchor_uuid.map(|u| u.as_uuid().to_string()),
        all_uuids: uuids.clone(),
        uuids,
    })
}

/// The `uuid` of a [`ConversationMessage`] as the boundary relink uses it
/// (TS messages carry a `uuid`; here the [`MessageId`] is the stable id).
/// BARE 8-4-4-4-12 form (`as_uuid()`, NOT the `msg:`-prefixed Display) — these
/// values must match on-disk JSONL line uuids, which are raw.
fn message_uuid(message: &ConversationMessage) -> String {
    match message {
        ConversationMessage::User { id, .. }
        | ConversationMessage::Assistant { id, .. }
        | ConversationMessage::System { id, .. } => id.as_uuid().to_string(),
    }
}

/// Construct a compact-boundary system message + metadata, populating
/// [`CompactBoundaryMetadata::preserved_segment`] from a preserved tail.
///
/// Same as [`create_compact_boundary`] but for the suffix-preserving
/// (`messagesToKeep`) path: the `kept_tail` is the verbatim tail kept after the
/// summary, and `anchor_uuid` is the last summary message's id (the splice
/// point). Mirrors `WAo(boundary, lastSummaryUuid, messagesToKeep)`
/// (`bin/claude.exe` offset 202984952). When `kept_tail` is empty the
/// `preserved_segment` is `None`, identical to [`create_compact_boundary`].
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn create_compact_boundary_with_preserved_tail(
    trigger: CompactTrigger,
    pre_tokens: u64,
    last_pre_compact_message_uuid: Option<MessageId>,
    user_context: Option<String>,
    messages_summarized: Option<u32>,
    discovered_tools: &[String],
    kept_tail: &[ConversationMessage],
    anchor_uuid: Option<&MessageId>,
) -> (ConversationMessage, CompactBoundaryMetadata) {
    let (mut marker, mut metadata) = create_compact_boundary(
        trigger,
        pre_tokens,
        last_pre_compact_message_uuid,
        user_context,
        messages_summarized,
        discovered_tools,
    );
    metadata.preserved_segment = preserved_segment_for_tail(kept_tail, anchor_uuid);
    metadata.preserved_messages = preserved_messages_for_tail(kept_tail, anchor_uuid);
    debug_assert!(marker.set_compact_metadata(metadata.clone()));
    (marker, metadata)
}

/// Whether `message` is a compact-boundary marker.
///
/// TS `isCompactBoundaryMessage`: `type === 'system' && subtype ===
/// 'compact_boundary'`. The body does not determine the message type.
#[must_use]
pub fn is_compact_boundary(message: &ConversationMessage) -> bool {
    matches!(
        message,
        ConversationMessage::System {
            subtype: Some(subtype),
            ..
        } if subtype == "compact_boundary"
    )
}

/// Index of the last compact boundary, scanning backward; `None` if none.
///
/// TS `findLastCompactBoundaryIndex` returns `-1` for "none"; the Rust idiom
/// is `Option<usize>`.
#[must_use]
pub fn find_last_compact_boundary_index(messages: &[ConversationMessage]) -> Option<usize> {
    messages.iter().rposition(is_compact_boundary)
}

/// Messages from the last compact boundary onward (including the boundary).
/// Returns all messages when no boundary exists.
///
/// TS `getMessagesAfterCompactBoundary`. The TS `includeSnipped` /
/// `projectSnippedView` path is the `HISTORY_SNIP` projection (a separate
/// subsystem ported in `snip.rs`) — out of scope here; this returns the raw
/// slice, matching TS with `includeSnipped: true` / `HISTORY_SNIP` off.
#[must_use]
pub fn get_messages_after_compact_boundary(
    messages: &[ConversationMessage],
) -> &[ConversationMessage] {
    match find_last_compact_boundary_index(messages) {
        Some(i) => &messages[i..],
        None => messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::ConversationMessage;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    fn boundary() -> ConversationMessage {
        create_compact_boundary(CompactTrigger::Manual, 0, None, None, None, &[]).0
    }

    #[test]
    fn create_populates_trigger_pre_tokens_and_discovered_tools() {
        let last = MessageId::new();
        let (marker, meta) = create_compact_boundary(
            CompactTrigger::Auto,
            123_456,
            Some(last),
            Some("ctx".to_string()),
            Some(7),
            &[
                "Zebra".to_string(),
                "alpha".to_string(),
                "Mango".to_string(),
            ],
        );

        assert_eq!(meta.trigger, CompactTrigger::Auto);
        assert_eq!(meta.pre_tokens, 123_456);
        assert_eq!(meta.user_context.as_deref(), Some("ctx"));
        assert_eq!(meta.messages_summarized, Some(7));
        // sorted (byte order: uppercase before lowercase)
        assert_eq!(
            meta.pre_compact_discovered_tools,
            vec![
                "Mango".to_string(),
                "Zebra".to_string(),
                "alpha".to_string()
            ]
        );
        assert_eq!(
            meta.logical_parent_uuid.as_deref(),
            Some(&*last.as_uuid().to_string())
        );
        assert_eq!(meta.preserved_segment, None);

        // The in-history marker carries the exact TS sentinel content.
        assert!(is_compact_boundary(&marker));
        match marker {
            ConversationMessage::System { content, .. } => {
                assert_eq!(content, "Conversation compacted");
            }
            _ => panic!("expected a System marker"),
        }
    }

    #[test]
    fn create_dedups_discovered_tools() {
        let (_m, meta) = create_compact_boundary(
            CompactTrigger::Manual,
            0,
            None,
            None,
            None,
            &[
                "b".to_string(),
                "a".to_string(),
                "b".to_string(),
                "a".to_string(),
            ],
        );
        assert_eq!(
            meta.pre_compact_discovered_tools,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn create_omits_logical_parent_when_absent() {
        let (_m, meta) = create_compact_boundary(CompactTrigger::Manual, 0, None, None, None, &[]);
        assert_eq!(meta.logical_parent_uuid, None);
    }

    #[test]
    fn trigger_as_str_matches_ts() {
        assert_eq!(CompactTrigger::Manual.as_str(), "manual");
        assert_eq!(CompactTrigger::Auto.as_str(), "auto");
    }

    #[test]
    fn is_boundary_distinguishes_system_marker_from_plain_system() {
        assert!(is_compact_boundary(&boundary()));
        let plain = ConversationMessage::System { api_system: None,
            id: MessageId::new(),
            content: "some other system text".to_string(),
            subtype: None,
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        assert!(!is_compact_boundary(&plain));
        assert!(!is_compact_boundary(&user("hi")));
    }

    #[test]
    fn matching_body_without_typed_subtype_is_an_ordinary_system_message() {
        let ordinary = ConversationMessage::System { api_system: None,
            id: MessageId::new(),
            content: BOUNDARY_CONTENT.to_string(),
            subtype: None,
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        assert!(!is_compact_boundary(&ordinary));
        let messages = vec![user("before"), ordinary, user("after")];
        assert_eq!(find_last_compact_boundary_index(&messages), None);
        assert_eq!(get_messages_after_compact_boundary(&messages), messages);
    }

    #[test]
    fn typed_boundary_predicate_and_history_match_actual_native_287() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/compact_boundary_2_1_287.json"
        ))
        .unwrap();
        assert_eq!(oracle["cases"].as_array().unwrap().len(), 6);
        for case in oracle["cases"].as_array().unwrap() {
            let messages = case["input"]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    let content = row["content"].as_str().unwrap().to_owned();
                    if row["type"] == "system" {
                        ConversationMessage::System { api_system: None,
                            id: MessageId::new(),
                            content,
                            subtype: row
                                .get("subtype")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned),
                            compact_metadata: None,
                            model_fallback: None,
                            refusal_fallback: None,
                        }
                    } else if row["isCompactSummary"] == true {
                        ConversationMessage::compact_summary(MessageId::new(), content)
                    } else {
                        ConversationMessage::user(MessageId::new(), content)
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(
                serde_json::json!(messages.iter().map(is_compact_boundary).collect::<Vec<_>>()),
                case["expected"]["boundaries"],
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::json!(messages
                    .iter()
                    .map(crate::selector::is_prior_compaction_artifact)
                    .collect::<Vec<_>>()),
                case["expected"]["artifacts"],
                "{}",
                case["name"]
            );
            let native_index = case["expected"]["boundaryIndex"].as_i64().unwrap();
            let expected_index = usize::try_from(native_index).ok();
            assert_eq!(find_last_compact_boundary_index(&messages), expected_index);
            assert_eq!(
                get_messages_after_compact_boundary(&messages),
                &messages[expected_index.unwrap_or(0)..],
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn active_goal_conversion_is_typed_and_roundtrips() {
        let goal = lingxi_core::session::ActiveGoalState {
            condition: "ship only when tests pass".to_string(),
            set_at: std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000),
            last_reason: Some("tests still running".to_string()),
            iterations: 3,
            tokens_at_start: 42,
            // NOT the default: a roundtrip that drops `origin` would still
            // compare equal if this were `User`.
            origin: lingxi_core::session::GoalOrigin::Restored,
        };
        let wire = compact_active_goal_from_engine(&goal);
        let json = serde_json::to_value(&wire).unwrap();
        let decoded: CompactActiveGoalState = serde_json::from_value(json).unwrap();
        assert_eq!(compact_active_goal_into_engine(decoded), goal);
    }

    #[test]
    fn find_last_index_returns_none_when_no_boundary() {
        let msgs = vec![user("a"), user("b")];
        assert_eq!(find_last_compact_boundary_index(&msgs), None);
    }

    #[test]
    fn find_last_index_returns_most_recent() {
        let msgs = vec![
            user("a"),
            boundary(), // index 1
            user("b"),
            boundary(), // index 3 — most recent
            user("c"),
        ];
        assert_eq!(find_last_compact_boundary_index(&msgs), Some(3));
    }

    #[test]
    fn slice_returns_all_when_no_boundary() {
        let msgs = vec![user("a"), user("b"), user("c")];
        let sliced = get_messages_after_compact_boundary(&msgs);
        assert_eq!(sliced.len(), 3);
        assert_eq!(sliced, &msgs[..]);
    }

    #[test]
    fn slice_starts_at_last_boundary_inclusive() {
        let msgs = vec![
            user("a"),
            boundary(),
            user("b"),
            boundary(), // index 3
            user("c"),
            user("d"),
        ];
        let sliced = get_messages_after_compact_boundary(&msgs);
        // boundary at 3 + the two trailing users = 3 messages, boundary first
        assert_eq!(sliced.len(), 3);
        assert!(is_compact_boundary(&sliced[0]));
        assert_eq!(sliced[1].text_content(), "c");
        assert_eq!(sliced[2].text_content(), "d");
    }

    #[test]
    fn slice_includes_boundary_when_it_is_last() {
        let msgs = vec![user("a"), boundary()];
        let sliced = get_messages_after_compact_boundary(&msgs);
        assert_eq!(sliced.len(), 1);
        assert!(is_compact_boundary(&sliced[0]));
    }

    // --- #58 preserved-segment (messagesToKeep relink) -------------------- //

    #[test]
    fn preserved_segment_none_for_empty_tail() {
        assert_eq!(preserved_segment_for_tail(&[], None), None);
    }

    #[test]
    fn preserved_segment_head_anchor_tail() {
        let anchor = MessageId::new();
        let m0 = user("kept-0");
        let m1 = user("kept-1");
        let m2 = user("kept-2");
        let head_id = match &m0 {
            ConversationMessage::User { id, .. } => id.as_uuid().to_string(),
            _ => unreachable!(),
        };
        let tail_id = match &m2 {
            ConversationMessage::User { id, .. } => id.as_uuid().to_string(),
            _ => unreachable!(),
        };
        let seg = preserved_segment_for_tail(&[m0, m1, m2], Some(&anchor))
            .expect("non-empty tail yields a segment");
        assert_eq!(seg.head_uuid.as_deref(), Some(head_id.as_str()));
        assert_eq!(seg.tail_uuid.as_deref(), Some(tail_id.as_str()));
        assert_eq!(
            seg.anchor_uuid.as_deref(),
            Some(&*anchor.as_uuid().to_string())
        );
    }

    #[test]
    fn boundary_with_preserved_tail_populates_segment() {
        let anchor = MessageId::new();
        let kept = vec![user("recent-1"), user("recent-2")];
        let (marker, meta) = create_compact_boundary_with_preserved_tail(
            CompactTrigger::Auto,
            1000,
            None,
            None,
            Some(3),
            &[],
            &kept,
            Some(&anchor),
        );
        assert!(is_compact_boundary(&marker));
        match &marker {
            ConversationMessage::System {
                compact_metadata: Some(marker_metadata),
                ..
            } => assert_eq!(marker_metadata, &meta),
            other => panic!("expected typed compact boundary, got {other:?}"),
        }
        let seg = meta.preserved_segment.expect("preserved segment set");
        assert_eq!(
            seg.anchor_uuid.as_deref(),
            Some(&*anchor.as_uuid().to_string())
        );
        assert!(seg.head_uuid.is_some());
        assert!(seg.tail_uuid.is_some());
        // The re-splice list mirrors the segment: same anchor, one uuid per
        // kept message, `uuids == allUuids` (this port keeps only chain
        // participants).
        let pm = meta.preserved_messages.expect("preserved messages set");
        assert_eq!(
            pm.anchor_uuid.as_deref(),
            Some(&*anchor.as_uuid().to_string())
        );
        assert_eq!(pm.uuids.len(), 2);
        assert_eq!(pm.uuids, pm.all_uuids);
        assert_eq!(pm.uuids.first(), seg.head_uuid.as_ref());
        assert_eq!(pm.uuids.last(), seg.tail_uuid.as_ref());
    }

    #[test]
    fn boundary_with_empty_tail_has_no_segment() {
        // Empty kept tail → preserved_segment None, identical to the plain
        // create_compact_boundary.
        let (_m, meta) = create_compact_boundary_with_preserved_tail(
            CompactTrigger::Manual,
            0,
            None,
            None,
            None,
            &[],
            &[],
            None,
        );
        assert_eq!(meta.preserved_segment, None);
        assert_eq!(meta.preserved_messages, None);
    }

    #[test]
    fn metadata_serializes_omitting_empty_optionals() {
        // Wire shape = CC's camelCase `compactMetadata` keys (real-transcript
        // verified: `{"trigger":"auto","preTokens":42,...}`).
        let (_m, meta) = create_compact_boundary(CompactTrigger::Auto, 42, None, None, None, &[]);
        let v = serde_json::to_value(&meta).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("trigger").and_then(|t| t.as_str()), Some("auto"));
        assert_eq!(
            obj.get("preTokens").and_then(serde_json::Value::as_u64),
            Some(42)
        );
        // empty / None fields are omitted from the wire shape
        assert!(!obj.contains_key("userContext"));
        assert!(!obj.contains_key("postTokens"));
        assert!(!obj.contains_key("cumulativeDroppedTokens"));
        assert!(!obj.contains_key("durationMs"));
        assert!(!obj.contains_key("messagesSummarized"));
        assert!(!obj.contains_key("preCompactDiscoveredTools"));
        assert!(!obj.contains_key("preservedSegment"));
        assert!(!obj.contains_key("preservedMessages"));
        assert!(!obj.contains_key("logicalParentUuid"));
    }

    #[test]
    fn metadata_wire_keys_are_camel_case_in_cc_order() {
        // Populated metadata serializes CC's exact camelCase keys, with the
        // nested preservedSegment/preservedMessages shapes
        // (`{headUuid,anchorUuid,tailUuid}` / `{anchorUuid,uuids,allUuids}`).
        let anchor = MessageId::new();
        let kept = vec![user("kept-a"), user("kept-b")];
        let (_m, mut meta) = create_compact_boundary_with_preserved_tail(
            CompactTrigger::Auto,
            1000,
            None,
            Some("ctx".to_string()),
            Some(9),
            &["Read".to_string()],
            &kept,
            Some(&anchor),
        );
        meta.post_tokens = Some(250);
        meta.cumulative_dropped_tokens = Some(750);
        meta.duration_ms = Some(1234);
        let v = serde_json::to_value(&meta).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "trigger",
                "preTokens",
                "postTokens",
                "cumulativeDroppedTokens",
                "durationMs",
                "userContext",
                "messagesSummarized",
                "preCompactDiscoveredTools",
                "preservedSegment",
                "preservedMessages",
            ]
        );
        let seg_keys: Vec<&str> = v["preservedSegment"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(seg_keys, vec!["headUuid", "anchorUuid", "tailUuid"]);
        let pm_keys: Vec<&str> = v["preservedMessages"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(pm_keys, vec!["anchorUuid", "uuids", "allUuids"]);
    }
}
