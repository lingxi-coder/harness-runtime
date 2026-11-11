//! Typed transcript-side request history metadata that is not part of the
//! provider-facing `ConversationMessage` shape.

use crate::types::utf16_json::Utf16JsonProjection;
use crate::types::MessageId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Raw Native outer fields consulted by task-delivery human-turn placement.
///
/// These values belong to a transcript row envelope, not the provider-facing
/// `ConversationMessage`. `None` means the property was absent; `Some(null)`
/// and `Some(false)` retain their source presence because Native distinguishes
/// absent fields from present values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDeliveryHistoryEnvelope {
    /// Raw `verifiedSlackHumanTurn` value. Native excludes the row only when
    /// this value is exactly boolean `true`.
    #[serde(
        default,
        rename = "verifiedSlackHumanTurn",
        skip_serializing_if = "Option::is_none"
    )]
    pub verified_slack_human_turn: Option<Utf16JsonProjection>,
    /// Raw `promptSource` value. Native's image-meta exception requires this
    /// property to be absent, regardless of the value when present.
    #[serde(
        default,
        rename = "promptSource",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_source: Option<Utf16JsonProjection>,
    /// Raw `toolUseResult` value. Any present value, including null, marks a
    /// tool-result user envelope for the Native human-turn predicate.
    #[serde(
        default,
        rename = "toolUseResult",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_use_result: Option<Utf16JsonProjection>,
}

impl TaskDeliveryHistoryEnvelope {
    /// True when none of the three Native envelope fields were present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.verified_slack_human_turn.is_none()
            && self.prompt_source.is_none()
            && self.tool_use_result.is_none()
    }
}

/// Native row identity and placement metadata needed to reconstruct the next
/// provider request after resume or a background-session fork.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRequestHistoryMetadata {
    /// User rows whose original JSONL `message.content` was a JSON string.
    /// A single text block alone does not establish this source root shape.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub scalar_user_content_ids: HashSet<MessageId>,
    /// Outer attachment-row UUIDs with Native's
    /// `placement:"before_closing_line"` marker.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub before_closing_line_ids: HashSet<MessageId>,
    /// Assistant row UUIDs that passed the complete source `ZCe` synthetic
    /// closing-line predicate when reconstructed from trusted transcript data.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub synthetic_closing_ids: HashSet<MessageId>,
    /// Raw outer `taskDelivery` values attached to their originating user row.
    /// This is a transcript carry path only; callers must not infer a value
    /// from task snapshots or presentation state.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub task_deliveries: HashMap<MessageId, Utf16JsonProjection>,
    /// Raw Native envelope fields used when classifying user rows before a
    /// queued task-delivery attachment. Entries are keyed by the user row UUID
    /// and omitted when all three source properties were absent.
    #[serde(
        default,
        rename = "taskDeliveryEnvelopes",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub task_delivery_envelopes: HashMap<MessageId, TaskDeliveryHistoryEnvelope>,
    /// Assistant usage and provider response identity, kept beside the frozen
    /// provider-facing messages. This lets context accounting use measured
    /// system/tool-prefix tokens after live turns, resume, and session forks.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub assistant_usage: HashMap<MessageId, Utf16JsonProjection>,
}

impl NativeRequestHistoryMetadata {
    /// Compare the exact request-placement and task-delivery provenance.
    /// Usage is a derived accounting sidecar: live logical response identities
    /// and transcript per-block identities may differ without changing the
    /// admitted request or its policy-relevant provenance.
    #[must_use]
    pub fn request_shape_matches(&self, other: &Self) -> bool {
        self.scalar_user_content_ids == other.scalar_user_content_ids
            && self.before_closing_line_ids == other.before_closing_line_ids
            && self.synthetic_closing_ids == other.synthetic_closing_ids
            && self.task_deliveries == other.task_deliveries
            && self.task_delivery_envelopes == other.task_delivery_envelopes
    }

    /// True when there is no request-history metadata to persist or transfer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scalar_user_content_ids.is_empty()
            && self.before_closing_line_ids.is_empty()
            && self.synthetic_closing_ids.is_empty()
            && self.task_deliveries.is_empty()
            && self.task_delivery_envelopes.is_empty()
            && self.assistant_usage.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_shape_ignores_derived_usage_but_preserves_all_request_provenance() {
        let id = MessageId::new();
        let base = NativeRequestHistoryMetadata::default();
        let mut derived = base.clone();
        derived.assistant_usage.insert(
            id,
            Utf16JsonProjection::parse(
                r#"{"providerMessageId":"provider-id","usage":{"input_tokens":200000}}"#,
            )
            .unwrap(),
        );
        assert!(base.request_shape_matches(&derived));
        assert_ne!(
            base, derived,
            "full metadata equality still includes accounting"
        );

        let mut changed = derived.clone();
        changed.scalar_user_content_ids.insert(id);
        assert!(!base.request_shape_matches(&changed));
        let mut changed = derived.clone();
        changed.before_closing_line_ids.insert(id);
        assert!(!base.request_shape_matches(&changed));
        let mut changed = derived.clone();
        changed.synthetic_closing_ids.insert(id);
        assert!(!base.request_shape_matches(&changed));
        let mut changed = derived.clone();
        changed
            .task_deliveries
            .insert(id, Utf16JsonProjection::parse(r#"{"id":"task"}"#).unwrap());
        assert!(!base.request_shape_matches(&changed));
        let mut changed = derived;
        changed.task_delivery_envelopes.insert(
            id,
            TaskDeliveryHistoryEnvelope {
                prompt_source: Some(Utf16JsonProjection::parse("null").unwrap()),
                ..Default::default()
            },
        );
        assert!(!base.request_shape_matches(&changed));
    }

    #[test]
    fn task_delivery_envelope_round_trip_preserves_missing_vs_null_and_false() {
        let row_id = MessageId::new();
        let envelope = TaskDeliveryHistoryEnvelope {
            verified_slack_human_turn: Some(Utf16JsonProjection::parse("null").unwrap()),
            prompt_source: Some(Utf16JsonProjection::parse("false").unwrap()),
            tool_use_result: Some(Utf16JsonProjection::parse("null").unwrap()),
        };
        let mut metadata = NativeRequestHistoryMetadata::default();
        metadata
            .task_delivery_envelopes
            .insert(row_id, envelope.clone());

        let encoded = serde_json::to_value(&metadata).unwrap();
        let decoded: NativeRequestHistoryMetadata = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded.task_delivery_envelopes[&row_id], envelope);
        assert!(decoded.task_delivery_envelopes[&row_id]
            .verified_slack_human_turn
            .as_ref()
            .unwrap()
            .at_pointer("")
            .unwrap()
            .is_null());
        assert_eq!(
            decoded.task_delivery_envelopes[&row_id]
                .prompt_source
                .as_ref()
                .unwrap()
                .at_pointer("")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        assert!(decoded.task_delivery_envelopes[&row_id]
            .tool_use_result
            .as_ref()
            .unwrap()
            .at_pointer("")
            .unwrap()
            .is_null());
        assert!(!decoded
            .task_delivery_envelopes
            .contains_key(&MessageId::new()));
    }

    #[test]
    fn scalar_user_source_shape_survives_fork_metadata_round_trip() {
        let id = MessageId::new();
        let mut metadata = NativeRequestHistoryMetadata::default();
        metadata.scalar_user_content_ids.insert(id);
        let encoded = serde_json::to_string(&metadata).unwrap();
        let restored: NativeRequestHistoryMetadata = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.scalar_user_content_ids, HashSet::from([id]));
        assert!(metadata.request_shape_matches(&restored));
        assert!(!restored.is_empty());
        let legacy: NativeRequestHistoryMetadata = serde_json::from_str("{}").unwrap();
        assert!(legacy.scalar_user_content_ids.is_empty());
    }
}
