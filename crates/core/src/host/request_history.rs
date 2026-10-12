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
}

impl NativeRequestHistoryMetadata {
    /// True when there is no request-history metadata to persist or transfer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.before_closing_line_ids.is_empty()
            && self.synthetic_closing_ids.is_empty()
            && self.task_deliveries.is_empty()
            && self.task_delivery_envelopes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
