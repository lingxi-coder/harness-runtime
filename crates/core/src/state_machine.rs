//! Conversation state machine. Each variant represents one distinct moment
//! in the agentic loop. The reducer transitions between these.
//!
//! M1.1 ships 4 states. Later plans add: `ToolUseReceived`,
//! `AwaitingPermission`, `AwaitingToolResult`, `AwaitingSubagent`,
//! `Compacting`, `HookBlocked`, `MemoryPrefetchInProgress`.

use crate::session::SessionState;
use crate::types::RequestId;
use serde::{Deserialize, Serialize};

/// One distinct moment in the agentic loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationState {
    /// No request is in flight; waiting for the next user input.
    Idle {
        /// Current session snapshot.
        session: SessionState,
    },
    /// A user message arrived and the request body is being assembled.
    AssemblingPrompt {
        /// Current session snapshot.
        session: SessionState,
        /// Raw text from the user that will drive the next request.
        user_message: String,
    },
    /// Request is dispatched; waiting for the API stream to open.
    AwaitingApiResponse {
        /// Current session snapshot.
        session: SessionState,
        /// Identifier of the in-flight request.
        request_id: RequestId,
    },
    /// The API stream is open and delta chunks are accumulating.
    StreamingResponse {
        /// Current session snapshot.
        session: SessionState,
        /// Identifier of the in-flight request.
        request_id: RequestId,
        /// Text accumulated from stream deltas so far.
        partial_text: String,
    },
    /// Terminal state — no further transitions are possible.
    Terminated {
        /// Final session snapshot.
        session: SessionState,
        /// Human-readable reason the conversation ended.
        reason: String,
    },
}

impl ConversationState {
    /// Borrow the session held by this state.
    #[must_use]
    pub fn session(&self) -> &SessionState {
        match self {
            Self::Idle { session }
            | Self::AssemblingPrompt { session, .. }
            | Self::AwaitingApiResponse { session, .. }
            | Self::StreamingResponse { session, .. }
            | Self::Terminated { session, .. } => session,
        }
    }

    /// True iff this state is `Terminated` and no further transitions apply.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminated { .. })
    }

    /// For `RecordUnexpectedEvent` diagnostics.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Idle { .. } => "Idle",
            Self::AssemblingPrompt { .. } => "AssemblingPrompt",
            Self::AwaitingApiResponse { .. } => "AwaitingApiResponse",
            Self::StreamingResponse { .. } => "StreamingResponse",
            Self::Terminated { .. } => "Terminated",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionId;

    #[test]
    fn idle_state_holds_session() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        let state = ConversationState::Idle {
            session: session.clone(),
        };
        assert_eq!(state.session().session_id, session.session_id);
    }

    #[test]
    fn terminated_is_terminal() {
        let session = SessionState::empty(SessionId::nil(), "x".into());
        let state = ConversationState::Terminated {
            session,
            reason: "ok".into(),
        };
        assert!(state.is_terminal());
    }
}
