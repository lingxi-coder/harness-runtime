//! Inputs to the reducer. Each Event is a single observable thing that
//! happened in the outside world (user typed, API streamed a chunk, ...).
//!
//! IDs/timestamps live in the events, not in the reducer — see D17 (purity).
//!
//! M1.1 ships a subset. Later plans extend this enum.

use crate::session::SessionState;
use crate::token::Usage;
use crate::types::{ConversationMessage, MessageId, RequestId, ToolUseId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single observable input to the reducer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    // — User input —
    /// A user submitted a chat message.
    UserMessage {
        /// Identifier assigned to the user's message.
        message_id: MessageId,
        /// Identifier for the request this message kicks off.
        request_id: RequestId,
        /// Raw text content from the user.
        content: String,
    },
    /// A user-role message injected by a JavaScript hook whose exact UTF-16
    /// code units include an isolated surrogate.
    UserMessageJsUtf16 {
        /// Identifier assigned to the injected message.
        message_id: MessageId,
        /// Identifier for the request this message kicks off.
        request_id: RequestId,
        /// Display-safe UTF-8 text.
        content: String,
        /// Authoritative JavaScript string units used by the model wire path.
        utf16_code_units: Vec<u16>,
    },
    /// An admitted subagent report. Its typed peer provenance never grants
    /// user authority or user-only attachment/slash-command processing.
    PeerMessage {
        envelope: crate::host::handback::HandbackEnvelope,
    },
    /// The user cancelled the in-flight request (e.g. Ctrl+C).
    UserInterrupt,
    /// The user requested to exit the session.
    UserExit,

    // — API responses —
    /// The API has accepted the request and the stream is opening.
    ApiStreamStart {
        /// Request the stream belongs to.
        request_id: RequestId,
    },
    /// A delta token chunk arrived on the stream.
    ApiStreamDelta {
        /// Request the chunk belongs to.
        request_id: RequestId,
        /// Decoded text fragment from the stream.
        text: String,
    },
    /// The stream completed cleanly with a final message and usage.
    ApiStreamEnd {
        /// Request that just finished.
        request_id: RequestId,
        /// Reconstructed final assistant message.
        final_message: ConversationMessage,
        /// Token accounting for this request.
        usage: Usage,
    },
    /// The API returned (or the transport raised) an error.
    ApiError {
        /// Request the error belongs to.
        request_id: RequestId,
        /// Structured payload describing the failure.
        error: ApiErrorPayload,
    },

    // — System —
    /// A persisted session was loaded back into memory.
    SessionLoaded(SessionState),

    // — Cost —
    /// A cost record was committed by the cost subsystem for a model call.
    CostRecorded {
        /// Model identifier the cost was recorded against.
        model: String,
        /// Opaque usage payload (schema owned by the `lingxi-cost` crate).
        usage: serde_json::Value,
        /// Computed cost of the call in nano USD (1e-9 USD).
        cost_nano_usd: u64,
    },
    /// Cumulative spend has crossed a configured budget threshold.
    BudgetThresholdReached {
        /// Threshold percentage that was crossed (e.g. `80` for 80%).
        pct: u32,
        /// Current spend in nano USD at the time of the event.
        current: u64,
        /// Configured budget limit in nano USD.
        limit: u64,
    },
    /// Cumulative spend has met or exceeded the configured budget limit.
    BudgetExceeded {
        /// Current spend in nano USD at the time of the event.
        current: u64,
        /// Configured budget limit in nano USD.
        limit: u64,
    },

    // — Permission —
    /// The permission subsystem granted a previously-pending tool call.
    PermissionGranted {
        /// `ToolUseId` of the granted call.
        call_id: ToolUseId,
    },
    /// The permission subsystem denied a previously-pending tool call.
    PermissionDenied {
        /// `ToolUseId` of the denied call.
        call_id: ToolUseId,
    },

    // — Secret —
    /// The secret scanner detected sensitive material crossing a boundary.
    SecretDetected {
        /// Boundary label where the match was observed.
        boundary: String,
        /// Identifier of the rule that matched.
        rule_id: String,
        /// Whether the scanner emitted a redacted copy of the content.
        redacted: bool,
    },
}

/// Structured payload for `Event::ApiError`.
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[error("api error: {message}")]
pub struct ApiErrorPayload {
    /// Coarse error class (e.g. `"rate_limit"`, `"overloaded"`).
    pub kind: String,
    /// Human-readable error message from the API or transport.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_event_serializes() {
        let e = Event::UserMessage {
            message_id: MessageId::nil(),
            request_id: RequestId::nil(),
            content: "hi".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("user_message"));
        assert!(s.contains("hi"));
    }
}
