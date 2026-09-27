//! The transport-agnostic event sink.
//!
//! [`ClientEventSink`] is the ONE seam the engine→DTO bridge writes to. Every
//! adapter component that produces a [`ClientEvent`] — the live-turn
//! [`crate::output_stream::AdapterOutputStream`] (F1-12), the permission gate
//! (F1-14), the turn wrapper (F1-13) — pushes through this trait, never to a
//! concrete transport.
//!
//! Both transports supply their own sink implementation:
//! - bridge-server (Electron) wraps an outbound WebSocket `Frame::Event` writer
//!   (F2),
//! - mobile (iOS/Android) wraps the `UniFFI` `ClientEventListener` callback (F3).
//!
//! Keeping the adapter coupled only to this trait — and never to a transport —
//! is what lets the SAME lowering logic feed both surfaces (governing decision
//! §0.1 / §0.2).

use async_trait::async_trait;
use client_protocol::events::ClientEvent;

/// A transport-agnostic destination for the [`ClientEvent`] DTOs the adapter
/// emits.
///
/// The single [`emit`](ClientEventSink::emit) method takes the event by value:
/// the adapter has already lowered the engine type to an owned DTO, and each
/// transport serializes/forwards it independently. The trait is object-safe and
/// always used behind an `Arc<dyn ClientEventSink>` so a connection-scoped sink
/// can be shared across the output stream, the permission gate, and the turn
/// wrapper.
///
/// `Send + Sync` are required because the sink is awaited from the turn future
/// and resolved from a separate transport task (see the F1-14 id-keyed gate).
#[async_trait]
pub trait ClientEventSink: Send + Sync {
    /// Forward one fully-lowered [`ClientEvent`] to the underlying transport.
    ///
    /// Implementations should be cheap and non-blocking on the engine task —
    /// e.g. enqueue onto an mpsc channel or invoke a foreign callback — so the
    /// streaming loop is never stalled on transport back-pressure.
    async fn emit(&self, event: ClientEvent);
}
