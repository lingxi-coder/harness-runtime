//! `ClientEventListener` — the outbound `UniFFI` callback interface (plan F3-02).
//!
//! This is the mobile transport's analog of the bridge-server's outbound
//! WebSocket `Frame::Event` writer: where bridge-server wraps an mpsc/socket in
//! a [`ClientEventSink`](crate::adapter::sink::ClientEventSink), mobile wraps a
//! foreign-supplied (Swift/Kotlin) callback object. It is the OUTBOUND sibling
//! of the existing INBOUND device callbacks (`CameraControl` / `AudioService`
//! / `SharingService`): Rust calls *out* into the host with each translated
//! [`ClientEvent`].
//!
//! ## Shape
//!
//! [`ClientEventListener`] is an async callback interface. The host registers
//! one listener per engine handle (F3-04); the adapter holds it as an
//! `Arc<dyn ClientEventListener>` wrapped in a [`ListenerSink`] so the SAME
//! lowering pipeline (`AdapterOutputStream` / turn wrapper / permission gate)
//! that feeds bridge-server feeds the mobile listener unchanged.
//!
//! ## Why a `ClientEventSink` adapter
//!
//! The adapter's producers only know [`ClientEventSink`]. [`ListenerSink`]
//! bridges that trait to the foreign callback: `emit` forwards straight to
//! `on_event`. This keeps the listener a pure transport detail — the lowering
//! code never names it — exactly mirroring `BridgeOutputStream` →
//! `TurnEvent`, but pushing to the listener instead of an mpsc channel
//! (governing decision §0.1 / §0.2; plan F3-02).

use std::sync::Arc;

use crate::protocol::{events::ClientEvent, listings::WorkflowProgressDto};
use async_trait::async_trait;

use crate::adapter::sink::ClientEventSink;

/// The outbound event callback the mobile host (Swift / Kotlin) implements and
/// registers with the engine handle. Every translated [`ClientEvent`] the
/// adapter produces is delivered here.
///
/// Under the `uniffi` feature this is a UniFFI **callback interface**: the
/// foreign side supplies an object implementing `on_event`, marshalled across
/// the FFI as an `Arc<dyn ClientEventListener>`. The method is `async` so the
/// foreign executor (the handle-owned tokio runtime, F3-07) drives delivery
/// without blocking the engine turn loop.
///
/// `Send + Sync` are required because the listener is shared across the engine
/// task (which emits) and is invoked from the adapter's connection-scoped sink.
#[cfg_attr(feature = "uniffi", uniffi::export(callback_interface))]
#[async_trait]
pub trait ClientEventListener: Send + Sync {
    /// Deliver one fully-lowered [`ClientEvent`] to the foreign host.
    ///
    /// Implementations must be cheap / non-blocking with respect to the engine
    /// task — they typically enqueue onto the host UI's event stream.
    async fn on_event(&self, event: ClientEvent);

    /// Deliver one structured workflow progress update directly to the host.
    ///
    /// This deliberately stays outside [`ClientEvent`]: that enum is already
    /// close to UniFFI's fixed metadata limit, while a dedicated callback also
    /// makes the live child-event channel explicit instead of encouraging
    /// clients to reconstruct state from transcript events.
    async fn on_workflow_progress(
        &self,
        _origin_session_id: String,
        _task_id: String,
        _run_id: String,
        _progress: WorkflowProgressDto,
    ) {
    }
}

/// Bridges a foreign [`ClientEventListener`] to the adapter's transport-agnostic
/// [`ClientEventSink`].
///
/// The adapter's event producers (`AdapterOutputStream`, the turn wrapper, the
/// permission gate) only ever write to a `ClientEventSink`. On the mobile
/// transport that sink is a [`ListenerSink`] wrapping the registered
/// `Arc<dyn ClientEventListener>`: each [`ClientEventSink::emit`] forwards
/// verbatim to [`ClientEventListener::on_event`]. This is the single seam where
/// the listener becomes a sink, so the lowering pipeline stays
/// transport-agnostic.
#[derive(Clone)]
pub struct ListenerSink {
    listener: Arc<dyn ClientEventListener>,
}

impl ListenerSink {
    /// Wrap a registered [`ClientEventListener`] as a [`ClientEventSink`].
    #[must_use]
    pub fn new(listener: Arc<dyn ClientEventListener>) -> Self {
        Self { listener }
    }

    /// Wrap a listener and return it as a ready-to-share `Arc<dyn ClientEventSink>`
    /// — the form every connection-scoped adapter component holds.
    #[must_use]
    pub fn arc(listener: Arc<dyn ClientEventListener>) -> Arc<dyn ClientEventSink> {
        Arc::new(Self::new(listener))
    }
}

#[async_trait]
impl ClientEventSink for ListenerSink {
    async fn emit(&self, event: ClientEvent) {
        self.listener.on_event(event).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::protocol::events::CostDto;
    use crate::protocol::events::{ClientEvent, ErrorKindDto, TurnOutcomeDto};
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use crate::adapter::sink::ClientEventSink;

    use super::{ClientEventListener, ListenerSink};

    /// A host-fake [`ClientEventListener`] that records every delivered event —
    /// the off-device stand-in for a Swift/Kotlin listener (plan F3-02).
    #[derive(Default)]
    struct FakeListener {
        received: Mutex<Vec<ClientEvent>>,
    }

    #[async_trait]
    impl ClientEventListener for FakeListener {
        async fn on_event(&self, event: ClientEvent) {
            self.received.lock().await.push(event);
        }
    }

    /// F3-02 walking proof: a translated [`ClientEvent`] pushed onto the
    /// adapter's [`ClientEventSink`] (a [`ListenerSink`]) is delivered to the
    /// registered foreign listener via `on_event`, in order, byte-identical.
    #[tokio::test]
    async fn listener_receives_translated_event() {
        let listener = Arc::new(FakeListener::default());

        // The adapter only ever sees a `ClientEventSink`; on mobile that sink is
        // a `ListenerSink` over the registered listener.
        let sink: Arc<dyn ClientEventSink> = ListenerSink::arc(listener.clone());

        let text = ClientEvent::TextDelta {
            text: "hello from the engine".to_string(),
        };
        let ended = ClientEvent::TurnEnded {
            outcome: TurnOutcomeDto::EndTurn,
            stop_reason: Some("end_turn".to_string()),
            cost: CostDto {
                total_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                api_calls: 0,
                session_duration_secs: 0,
                formatted: "$0.00".to_string(),
            },
        };

        sink.emit(text.clone()).await;
        sink.emit(ended.clone()).await;

        let got = listener.received.lock().await.clone();
        assert_eq!(got, vec![text, ended]);
    }

    /// The listener is usable behind an `Arc<dyn ClientEventListener>` and the
    /// `ListenerSink` is object-safe behind `Arc<dyn ClientEventSink>` — the
    /// exact forms the engine handle (F3-04) holds. Compile-time guarantee.
    #[tokio::test]
    async fn listener_sink_is_object_safe() {
        let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
        let sink: Arc<dyn ClientEventSink> = ListenerSink::arc(listener);
        sink.emit(ClientEvent::Error {
            kind: ErrorKindDto::Internal,
            message: "probe".to_string(),
        })
        .await;
    }
}
