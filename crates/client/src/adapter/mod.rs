//! Runtime-to-client DTO conversion, event sinks and permission callbacks.
//! Enabled by the `adapter` feature.

#![forbid(unsafe_code)]

pub mod ask_user_question_broker;
pub mod computer_access_broker;
pub mod controls;
pub mod listener;
pub mod lowering;
pub mod output_stream;
pub mod permission_gate;
pub mod sink;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tool_display;
pub mod turn;

pub use ask_user_question_broker::AskUserQuestionBroker;
pub use computer_access_broker::{ComputerAccessBroker, ComputerAccessRequestSink};
pub use listener::{ClientEventListener, ListenerSink};
pub use output_stream::AdapterOutputStream;
pub use permission_gate::{
    AdapterPermissionGate, PermissionRequestSink, DEFAULT_PERMISSION_TIMEOUT,
};
pub use sink::ClientEventSink;
#[cfg(any(test, feature = "test-support"))]
pub use test_support::MockSink;
pub use turn::{
    error_kind_for, map_orchestrator_error, message_complete_event, synthesize_message,
    turn_started_event, TurnEventEmitter,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::protocol::events::{ClientEvent, ErrorKindDto};

    use super::sink::ClientEventSink;
    use super::test_support::MockSink;

    /// The sink is object-safe and usable behind an `Arc<dyn ClientEventSink>` —
    /// the form every connection-scoped adapter component holds. This is a
    /// compile-time guarantee: if `ClientEventSink` ever became non-object-safe
    /// this test would fail to build.
    #[test]
    fn sink_trait_object_compiles() {
        let sink: Arc<dyn ClientEventSink> = MockSink::arc();
        // Use the trait object so the coercion is not optimized away.
        let _: &dyn ClientEventSink = &*sink;
    }

    /// The `MockSink` captures emitted events in emission order so later tasks
    /// can assert the live-turn / permission feed.
    #[tokio::test]
    async fn mock_sink_captures_emitted_events_in_order() {
        let sink = MockSink::arc();
        assert!(sink.is_empty().await);

        let dyn_sink: Arc<dyn ClientEventSink> = sink.clone();
        dyn_sink
            .emit(ClientEvent::Error {
                kind: ErrorKindDto::Transport,
                message: "first".to_string(),
            })
            .await;
        dyn_sink
            .emit(ClientEvent::Error {
                kind: ErrorKindDto::Internal,
                message: "second".to_string(),
            })
            .await;

        let captured = sink.events().await;
        assert_eq!(captured.len(), 2);
        assert_eq!(sink.len().await, 2);
        assert!(!sink.is_empty().await);
        assert_eq!(
            captured[0],
            ClientEvent::Error {
                kind: ErrorKindDto::Transport,
                message: "first".to_string(),
            }
        );
        assert_eq!(
            captured[1],
            ClientEvent::Error {
                kind: ErrorKindDto::Internal,
                message: "second".to_string(),
            }
        );
    }
}
