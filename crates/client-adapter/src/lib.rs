//! `client-adapter` — the single engine→DTO bridge.
//!
//! This crate is the ONLY place the engine's runtime types are lowered into the
//! `client-protocol` wire contract (governing decision §0.2). It defines the
//! transport-agnostic [`ClientEventSink`] both transports write to, and (in the
//! later F1 tasks) the pure lowering fns, the `AdapterOutputStream`, the
//! live-turn wrapper, and the id-keyed `AdapterPermissionGate`.
//!
//! ## Where `serde_json::Value` is allowed
//!
//! `client-protocol` deliberately omits `serde_json` so `Value` never enters
//! the contract (it is not UniFFI-representable — §0.4). `client-adapter` is the
//! one crate that DOES depend on `serde_json`: it is where a tool's `Value`
//! input/result is lowered to the `input_json` / `result_json` JSON String wire
//! fields.
//!
//! ## Reference templates — copy, do not import
//!
//! `tui`'s `BridgeOutputStream` (`tui/src/events/orchestrator_bridge.rs`) and
//! `TuiPermissionGate` (`tui/src/permission_bridge.rs`) are the proven shapes
//! the adapter mirrors. They are COPIED, never imported: this crate has no `tui`
//! dependency (plan F1-10), keeping the engine-tier free of a UI edge.
//!
//! At F1-10 only the [`sink`] module and the [`test_support`] mock exist;
//! subsequent F1-* tasks fill in `lowering`, `output_stream`, `turn`, and
//! `permission_gate`.

#![forbid(unsafe_code)]

pub mod ask_user_question_broker;
pub mod computer_access_broker;
pub mod controls;
pub mod listener;
pub mod lowering;
pub mod output_stream;
pub mod permission_gate;
pub mod sink;
pub mod test_support;
pub mod tool_display;
pub mod turn;

pub use ask_user_question_broker::BridgeAskUserQuestionBroker;
pub use computer_access_broker::{BridgeComputerAccessBroker, ComputerAccessRequestSink};
pub use listener::{ClientEventListener, ListenerSink};
pub use output_stream::AdapterOutputStream;
pub use permission_gate::{
    AdapterPermissionGate, PermissionRequestSink, DEFAULT_PERMISSION_TIMEOUT,
};
pub use sink::ClientEventSink;
pub use test_support::MockSink;
pub use turn::{
    error_kind_for, map_orchestrator_error, message_complete_event, synthesize_message,
    turn_started_event, TurnWrapper,
};

// UniFFI scaffolding (F3-02). The `#[uniffi::export(callback_interface)]` on
// `ClientEventListener` (in `listener`) registers its callback metadata through
// this macro. It compiles ONLY under the `uniffi` feature; the default
// (bridge-server / host-test) build never includes it. The aggregating cdylib
// crate (ios-framework / android-aar) re-exports this scaffolding so the
// listener's FFI symbols land in the final library.
#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use client_protocol::events::{ClientEvent, ErrorKindDto};

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
