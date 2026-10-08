//! `MailboxRouterHandle` — narrow trait abstracting
//! `coordinator::MailboxRouter::route` so `SendMessageTool` in `lingxi-tools`
//! can dispatch without taking a cyclic dep on `lingxi-coordinator`.
//!
//! Concrete impl lives in `lingxi-coordinator`. Tests inject a recording mock.

use crate::types::utf16_json::Utf16JsonProjection;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use thiserror::Error;

/// One message routed across teammates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MailboxMessage {
    /// Caller-assigned message id for idempotency.
    pub message_id: String,
    /// Plain-text message body, preserving the original UTF-16 source units.
    #[serde(with = "crate::types::utf16_json::raw_json_serde")]
    pub content: Utf16JsonProjection,
    /// Optional model-visible summary, preserving source units and the current
    /// JSON contract (`null` when absent; missing input defaults to `None`).
    #[serde(
        default,
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub summary: Option<Utf16JsonProjection>,
    /// Wall-clock send time.
    pub timestamp: SystemTime,
    /// Sender's assigned teammate color, if any (claude-code `getTeammateColor()`,
    /// `TaskUpdateTool.ts:279,294`). UI-only metadata carried on the mailbox
    /// message so the recipient can colorize the sender. `None` when the sender
    /// has no assigned color (e.g. the main-thread leader / `'team-lead'`), in
    /// which case the field is omitted from the serialized message — byte-faithful
    /// with claude-code's `color: undefined` (the key is dropped by JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// Ack returned by [`MailboxRouterHandle::route`].
///
/// `claim_window_secs` is byte-locked to `30` — the spec §7 line 498
/// teammate-claim window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteAck {
    /// Wall-clock time the recipient mailbox accepted the message.
    pub claimed_at: SystemTime,
    /// Window during which the recipient must acknowledge. Locked to 30s.
    pub claim_window_secs: u64,
}

/// Failure modes for [`MailboxRouterHandle::route`].
#[derive(Debug, Error)]
pub enum MailboxError {
    /// The recipient is not registered with the router.
    #[error("Mailbox: recipient not found: {0}")]
    NotFound(String),
    /// The recipient's inbox is full.
    #[error("Mailbox: inbox full")]
    Full,
    /// The recipient's inbox is closed.
    #[error("Mailbox: closed")]
    Closed,
    /// Any other internal failure.
    #[error("Mailbox: internal error: {0}")]
    Internal(String),
}

/// Route-a-message seam used by `SendMessageTool`.
#[async_trait]
pub trait MailboxRouterHandle: Send + Sync {
    /// Assigned team palette color for a currently registered identity.
    async fn teammate_color(&self, _name: &str) -> Option<String> {
        None
    }

    /// Route `message` from `from_agent` to `to_agent`.
    async fn route(
        &self,
        from_agent: &str,
        to_agent: &str,
        message: MailboxMessage,
    ) -> Result<RouteAck, MailboxError>;

    /// Fan one message out to every registered teammate except the sender.
    /// Returns the display names of mailboxes that accepted the message.
    ///
    /// The default fails closed so hosts that only implement point-to-point
    /// routing never report a fabricated broadcast success.
    async fn broadcast(
        &self,
        _from_agent: &str,
        _message: MailboxMessage,
    ) -> Result<Vec<String>, MailboxError> {
        Err(MailboxError::Internal(
            "broadcast routing is not available on this host".to_string(),
        ))
    }

    /// Snapshot the currently named recipients addressable through this router.
    /// Default empty so non-coordinator hosts do not need to implement it.
    async fn named_recipients(&self) -> Vec<(String, crate::types::AgentId)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn MailboxRouterHandle>> = None;
    }

    #[test]
    fn color_field_is_omitted_when_none() {
        // claude-code `getTeammateColor()` returns `undefined` for the main-thread
        // leader; the `color: undefined` key is then dropped from the serialized
        // mailbox message. A `None` color must serialize to NO `color` key.
        let msg = MailboxMessage {
            message_id: "m1".into(),
            content: "hi".into(),
            summary: None,
            timestamp: SystemTime::UNIX_EPOCH,
            color: None,
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert!(
            v.get("color").is_none(),
            "a None color must be omitted from the wire form"
        );
    }

    #[test]
    fn color_field_round_trips_when_present() {
        let msg = MailboxMessage {
            message_id: "m1".into(),
            content: "hi".into(),
            summary: None,
            timestamp: SystemTime::UNIX_EPOCH,
            color: Some("cyan".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["color"], "cyan");
        let back: MailboxMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back.color.as_deref(), Some("cyan"));
    }

    #[test]
    fn content_preserves_unpaired_utf16_on_the_json_boundary() {
        let content = Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        let summary = Utf16JsonProjection::root_string("S�".into(), vec![0x53, 0xd801]).unwrap();
        let msg = MailboxMessage {
            message_id: "m1".into(),
            content,
            summary: Some(summary),
            timestamp: SystemTime::UNIX_EPOCH,
            color: None,
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        assert!(encoded.contains(r#""content":"\ud800""#), "{encoded}");
        assert!(encoded.contains(r#""summary":"S\ud801""#), "{encoded}");
        let decoded: MailboxMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.content.string_units(""), Some(vec![0xd800]));
        assert_eq!(
            decoded
                .summary
                .as_ref()
                .and_then(|value| value.string_units("")),
            Some(vec![0x53, 0xd801])
        );
    }

    #[test]
    fn missing_summary_defaults_and_none_serializes_as_null() {
        let msg = MailboxMessage {
            message_id: "m1".into(),
            content: "hi".into(),
            summary: None,
            timestamp: SystemTime::UNIX_EPOCH,
            color: None,
        };
        let mut value = serde_json::to_value(&msg).unwrap();
        assert!(value["summary"].is_null());
        value.as_object_mut().unwrap().remove("summary");
        let decoded: MailboxMessage = serde_json::from_value(value).unwrap();
        assert!(decoded.summary.is_none());
    }
}
