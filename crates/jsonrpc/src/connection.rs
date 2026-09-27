//! High-level builder wrapping a transport (`Stream<Bytes>` + `Sink<Bytes>`)
//! into a `Router` + `Dispatcher` + `Broker` triplet — the API every
//! protocol-specific consumer crate uses.
//!
//! Three layers of constructors are exposed:
//!
//! 1. `Connection::builder(codec).build(stream, sink)` — the canonical path.
//! 2. Five convenience constructors over `AsyncRead`/`AsyncWrite`,
//!    `Stream<Bytes>`/`Sink<Bytes>`, pre-decoded `Message` streams, and raw
//!    mpsc channels (added during the M2-02a cross-review so M2-02b/c/d and
//!    M2-03 can build a `Connection` without reaching into the builder).
//! 3. (External) consumers wrap one of the above for their transport's
//!    quirks (e.g. WebSocket text frames → `from_message_streams`).

use std::any::TypeId;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::{Decoder, Encoder};

use crate::broker::{spawn as spawn_broker, BrokerError, BrokerHandle};
use crate::codec::CodecError;
use crate::inbound::{BoxedHandler, Dispatcher};
use crate::messages::{Message, Notification};
use crate::router::{Router, RouterError, StartedCall, DEFAULT_STARTING_REQUEST_ID};

/// Connection-level error variants — surfaces the broker, router, codec, and
/// I/O error types through a single enum the public API can return.
#[derive(Debug, Error)]
pub enum ConnectionError {
    /// Outbound router error (timeout, remote error, writer closed, serde).
    #[error("router: {0}")]
    Router(#[from] RouterError),
    /// Broker error (codec, io).
    #[error("broker: {0}")]
    Broker(#[from] BrokerError),
    /// Codec error surfaced outside the broker context.
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
}

/// Wire framing selector for builders that wrap raw byte channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// NDJSON: one JSON object per `\n`.
    Lines,
    /// LSP-style: `Content-Length: N\r\n\r\n<json>`.
    ContentLength,
}

/// Alias for `Mode` — kept for parity with the JS protocol layer naming.
pub type ConnectionMode = Mode;

/// Builder for a `Connection`. Use `Connection::builder()` to construct one,
/// then chain `.default_timeout(d)`, and finally `.build(stream, sink)`.
#[must_use]
pub struct ConnectionBuilder<C> {
    codec: C,
    default_timeout: Duration,
    initial_request_id: i64,
}

impl<C> ConnectionBuilder<C> {
    /// Override the default per-call timeout (default: 60s).
    pub fn default_timeout(mut self, d: Duration) -> Self {
        self.default_timeout = d;
        self
    }

    /// Build the connection. Spawns broker tasks and returns the `Connection`
    /// handle; dropping the handle drops the router (eventually aborting
    /// broker tasks once their queues drain).
    pub fn build<S, K>(self, inbound: S, outbound: K) -> Connection
    where
        S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
        K: futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
        C: Encoder<Message, Error = CodecError>
            + Decoder<Item = Message, Error = CodecError>
            + Send
            + 'static,
    {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx)
            .with_initial_request_id(self.initial_request_id)
            .with_default_timeout(self.default_timeout);
        let dispatcher = Dispatcher::new();
        let (broker, notif_rx) = spawn_broker(
            inbound,
            outbound,
            self.codec,
            router.clone(),
            dispatcher.clone(),
            outbound_rx,
        );
        Connection {
            router,
            dispatcher,
            broker: Arc::new(broker),
            notifications: notif_rx,
        }
    }
}

/// High-level JSON-RPC connection. Clone-cheap (`Arc` internals). Drop the
/// last clone to release the underlying broker handle.
pub struct Connection {
    router: Router,
    dispatcher: Dispatcher,
    broker: Arc<BrokerHandle>,
    /// Initial notifications receiver — call `notifications()` to resubscribe.
    notifications: broadcast::Receiver<Notification>,
}

impl Connection {
    /// Start a builder. Provide a codec — there is no default.
    pub fn builder<C: 'static>(codec: C) -> ConnectionBuilder<C> {
        ConnectionBuilder {
            codec,
            default_timeout: crate::router::DEFAULT_TIMEOUT,
            initial_request_id: if TypeId::of::<C>() == TypeId::of::<crate::codec::LspCodec>() {
                0
            } else {
                DEFAULT_STARTING_REQUEST_ID
            },
        }
    }

    // ---------------------------------------------------------------------
    // Convenience constructors used by M2-02b/c/d and M2-03. Each delegates
    // to `Connection::builder(codec).build(stream, sink)` after framing the
    // caller-supplied I/O appropriately. See "Decisions appended after
    // cross-review" in `docs/superpowers/plans/2026-05-23-m2-02a-jsonrpc.md`.
    // ---------------------------------------------------------------------

    /// Build a Connection that reads/writes NDJSON over `AsyncRead`/`AsyncWrite`.
    pub fn new_line_delimited<R, W>(reader: R, writer: W) -> Connection
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        use crate::codec::LineCodec;
        let (stream, sink) = async_io_to_stream_sink(reader, writer);
        Connection::builder(LineCodec::default()).build(stream, sink)
    }

    /// Build a Connection that reads/writes LSP-style `Content-Length` frames.
    pub fn new_lsp<R, W>(reader: R, writer: W) -> Connection
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        use crate::codec::LspCodec;
        let (stream, sink) = async_io_to_stream_sink(reader, writer);
        Connection::builder(LspCodec::default()).build(stream, sink)
    }

    /// Build a Connection from a pre-framed `Stream<Bytes>`/`Sink<Bytes>` pair (NDJSON).
    ///
    /// For LSP framing, callers should use `new_lsp`; this default uses
    /// `LineCodec`.
    pub fn from_stream_sink<S, K>(stream: S, sink: K) -> Connection
    where
        S: futures::Stream<Item = bytes::Bytes> + Send + Unpin + 'static,
        K: futures::Sink<bytes::Bytes, Error = std::io::Error> + Send + Unpin + 'static,
    {
        use crate::codec::LineCodec;
        use futures::StreamExt;
        // Wrap raw `Stream<Bytes>` into `Stream<Result<Bytes, io::Error>>`.
        let inbound = stream.map(Ok::<bytes::Bytes, std::io::Error>);
        Connection::builder(LineCodec::default()).build(inbound, sink)
    }

    /// Build a Connection from already-decoded `Message` streams (used by
    /// WebSocket transports where the wire frames are JSON text and decoding
    /// happens before the message reaches the JSON-RPC layer).
    pub fn from_message_streams<S, K>(inbound: S, outbound: K) -> Connection
    where
        S: futures::Stream<Item = Message> + Send + Unpin + 'static,
        K: futures::Sink<Message, Error = ConnectionError> + Send + Unpin + 'static,
    {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx);
        let dispatcher = Dispatcher::new();
        let (broker, notif_rx) = crate::broker::spawn_typed(
            inbound,
            outbound,
            router.clone(),
            dispatcher.clone(),
            outbound_rx,
        );
        Connection {
            router,
            dispatcher,
            broker: Arc::new(broker),
            notifications: notif_rx,
        }
    }

    /// Build a Connection from raw `Bytes` mpsc channels with explicit framing mode.
    #[must_use]
    pub fn new_streams(
        read_rx: tokio::sync::mpsc::Receiver<Bytes>,
        write_tx: tokio::sync::mpsc::Sender<Bytes>,
        mode: Mode,
    ) -> Connection {
        use crate::codec::{LineCodec, LspCodec};
        use futures::stream::unfold;
        let inbound = Box::pin(unfold(read_rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|v| (Ok::<Bytes, std::io::Error>(v), rx))
        }));
        let outbound = Box::pin(futures::sink::unfold(
            write_tx,
            |tx, item: Bytes| async move {
                tx.send(item)
                    .await
                    .map_err(|_| std::io::Error::other("writer closed"))?;
                Ok::<_, std::io::Error>(tx)
            },
        ));
        match mode {
            Mode::Lines => Connection::builder(LineCodec::default()).build(inbound, outbound),
            Mode::ContentLength => {
                Connection::builder(LspCodec::default()).build(inbound, outbound)
            }
        }
    }

    /// Send an outbound request and await the typed response.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, ConnectionError> {
        Ok(self.router.call(method, params).await?)
    }

    /// Send an outbound request without applying a local timeout.
    pub async fn call_unbounded<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, ConnectionError> {
        Ok(self.router.call_unbounded(method, params).await?)
    }

    /// Start an outbound request without a local timeout and return the
    /// assigned JSON-RPC id plus a later wait handle.
    pub fn start_call_unbounded<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<StartedCall, ConnectionError> {
        Ok(self.router.start_call_unbounded(method, params)?)
    }

    /// Send an outbound request with an explicit per-call timeout.
    pub async fn call_with_timeout<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<R, ConnectionError> {
        Ok(self
            .router
            .call_with_timeout(method, params, timeout)
            .await?)
    }

    /// Send a disposable request while surfacing a response with a mismatched
    /// id. Protocol negotiation uses this on a sibling connection with one
    /// pending probe; ordinary calls continue to ignore unknown ids.
    pub async fn call_with_timeout_probe<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<R, ConnectionError> {
        Ok(self
            .router
            .call_with_timeout_probe(method, params, timeout)
            .await?)
    }

    /// Send an outbound notification (fire-and-forget). Returns once the
    /// writer task has accepted the message into its outbound queue.
    pub fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), ConnectionError> {
        Ok(self.router.notify(method, params)?)
    }

    /// Register an inbound handler for `method`. Replaces any existing
    /// handler for that method.
    pub async fn register_handler(&self, method: impl Into<String>, handler: BoxedHandler) {
        self.dispatcher.register(method, handler).await;
    }

    /// Subscribe to inbound notifications. Each call returns a fresh
    /// broadcast receiver that starts seeing notifications from this point
    /// onward.
    #[must_use]
    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications.resubscribe()
    }

    /// Whether the connection broker has terminated because the peer closed,
    /// I/O failed, or a framing/codec error disconnected it.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.broker.is_finished()
    }

    /// Abort the broker tasks. After this, all outbound calls fail with
    /// `RouterError::WriterClosed`.
    pub fn close(&self) {
        self.router.close();
        self.broker.abort();
    }
}

/// Internal helper: convert an `AsyncRead`/`AsyncWrite` pair into the
/// `Stream<Result<Bytes, io::Error>>` + `Sink<Bytes, Error = io::Error>` shape
/// the builder expects. Uses `tokio_util::io::ReaderStream` (which produces
/// `Bytes` chunks for any read size) and a `BytesCodec`-driven `FramedWrite`
/// for the sink half so we keep one consistent abstraction.
fn async_io_to_stream_sink<R, W>(
    reader: R,
    writer: W,
) -> (
    impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
    impl futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
)
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    use futures::SinkExt;
    use tokio_util::codec::{BytesCodec, FramedWrite};
    use tokio_util::io::ReaderStream;
    // `ReaderStream` yields `Bytes` chunks (already an Item = Result<Bytes, io::Error>).
    let stream = ReaderStream::new(reader);
    // `FramedWrite<W, BytesCodec>` accepts `BytesMut` items; wrap it so it
    // accepts `Bytes` (which the broker writer task hands us).
    let framed = FramedWrite::new(writer, BytesCodec::new());
    let sink = framed
        .with(|b: Bytes| async move { Ok::<_, std::io::Error>(bytes::BytesMut::from(b.as_ref())) });
    (stream, Box::pin(sink))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineCodec;
    use crate::inbound::InboundHandler;
    use crate::messages::{Request, Response};
    use async_trait::async_trait;
    use serde_json::json;

    /// Convenience: an in-memory bidirectional `Bytes` pipe — both halves usable
    /// as Stream/Sink for two ends of a Connection.
    #[allow(clippy::type_complexity)]
    fn duplex_pipe() -> (
        // end A: stream + sink
        impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
        impl futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
        // end B: stream + sink
        impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
        impl futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
    ) {
        use futures::stream::unfold;
        let (a_to_b_tx, a_to_b_rx) = mpsc::unbounded_channel::<Bytes>();
        let (b_to_a_tx, b_to_a_rx) = mpsc::unbounded_channel::<Bytes>();

        let a_in = Box::pin(unfold(b_to_a_rx, |mut rx| async move {
            rx.recv().await.map(|v| (Ok(v), rx))
        }));
        let a_out = Box::pin(futures::sink::unfold(
            a_to_b_tx,
            |tx, item: Bytes| async move {
                tx.send(item)
                    .map_err(|_| std::io::Error::other("sink closed"))?;
                Ok::<_, std::io::Error>(tx)
            },
        ));
        let b_in = Box::pin(unfold(a_to_b_rx, |mut rx| async move {
            rx.recv().await.map(|v| (Ok(v), rx))
        }));
        let b_out = Box::pin(futures::sink::unfold(
            b_to_a_tx,
            |tx, item: Bytes| async move {
                tx.send(item)
                    .map_err(|_| std::io::Error::other("sink closed"))?;
                Ok::<_, std::io::Error>(tx)
            },
        ));

        (a_in, a_out, b_in, b_out)
    }

    struct Echo;
    #[async_trait]
    impl InboundHandler for Echo {
        async fn handle(&self, req: Request) -> Response {
            Response::success(req.id, req.params.unwrap_or(json!(null)))
        }
    }

    #[tokio::test]
    async fn connection_call_response_against_peer_handler() {
        let (a_in, a_out, b_in, b_out) = duplex_pipe();

        let conn_a = Connection::builder(LineCodec::default()).build(a_in, a_out);
        let conn_b = Connection::builder(LineCodec::default()).build(b_in, b_out);

        // B side answers `echo`.
        conn_b.register_handler("echo", Arc::new(Echo)).await;

        // A side calls `echo`.
        let result: serde_json::Value = conn_a.call("echo", json!({"x": 1})).await.unwrap();
        assert_eq!(result, json!({"x": 1}));
    }

    #[tokio::test]
    async fn connection_notify_arrives_at_peer_broadcast() {
        let (a_in, a_out, b_in, b_out) = duplex_pipe();

        let conn_a = Connection::builder(LineCodec::default()).build(a_in, a_out);
        let conn_b = Connection::builder(LineCodec::default()).build(b_in, b_out);

        let mut sub = conn_b.notifications();
        conn_a.notify("evt", json!({"k": "v"})).unwrap();
        let n = sub.recv().await.unwrap();
        assert_eq!(n.method, "evt");
        assert_eq!(n.params, Some(json!({"k": "v"})));
    }

    // -----------------------------------------------------------------
    // Convenience-constructor roundtrip tests — each builder variant must
    // produce a `Connection` that successfully sends a `ping` call through
    // to a peer registered with an echo-style handler.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn new_line_delimited_roundtrips_a_call() {
        let (a, b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);
        let (b_r, b_w) = tokio::io::split(b);
        let conn_a = Connection::new_line_delimited(a_r, a_w);
        let conn_b = Connection::new_line_delimited(b_r, b_w);
        conn_b.register_handler("ping", Arc::new(Echo)).await;
        let _: serde_json::Value = conn_a.call("ping", json!({})).await.expect("roundtrip");
    }

    #[tokio::test]
    async fn new_lsp_roundtrips_a_call() {
        let (a, b) = tokio::io::duplex(8192);
        let (a_r, a_w) = tokio::io::split(a);
        let (b_r, b_w) = tokio::io::split(b);
        let conn_a = Connection::new_lsp(a_r, a_w);
        let conn_b = Connection::new_lsp(b_r, b_w);
        conn_b.register_handler("ping", Arc::new(Echo)).await;
        let _: serde_json::Value = conn_a.call("ping", json!({})).await.expect("roundtrip");
    }

    #[tokio::test]
    async fn from_stream_sink_roundtrips_a_call() {
        use futures::StreamExt;
        let (a_in, a_out, b_in, b_out) = duplex_pipe();
        // duplex_pipe returns Stream<Result<Bytes,_>>/Sink — we wrap them into
        // the bare Stream<Bytes>/Sink<Bytes> shape `from_stream_sink` expects.
        let a_in = Box::pin(a_in.filter_map(|r| async move { r.ok() }));
        let b_in = Box::pin(b_in.filter_map(|r| async move { r.ok() }));
        let conn_a = Connection::from_stream_sink(a_in, a_out);
        let conn_b = Connection::from_stream_sink(b_in, b_out);
        conn_b.register_handler("ping", Arc::new(Echo)).await;
        let _: serde_json::Value = conn_a.call("ping", json!({})).await.expect("roundtrip");
    }

    #[tokio::test]
    async fn from_message_streams_roundtrips_a_call() {
        let (a_tx, a_rx) = mpsc::unbounded_channel::<Message>();
        let (b_tx, b_rx) = mpsc::unbounded_channel::<Message>();
        let a_in =
            futures::stream::unfold(
                b_rx,
                |mut rx| async move { rx.recv().await.map(|m| (m, rx)) },
            );
        let b_in =
            futures::stream::unfold(
                a_rx,
                |mut rx| async move { rx.recv().await.map(|m| (m, rx)) },
            );
        let a_out = futures::sink::unfold(a_tx, |tx, m: Message| async move {
            tx.send(m)
                .map_err(|_| ConnectionError::Broker(BrokerError::Join("sink closed".into())))?;
            Ok::<_, ConnectionError>(tx)
        });
        let b_out = futures::sink::unfold(b_tx, |tx, m: Message| async move {
            tx.send(m)
                .map_err(|_| ConnectionError::Broker(BrokerError::Join("sink closed".into())))?;
            Ok::<_, ConnectionError>(tx)
        });
        let conn_a = Connection::from_message_streams(Box::pin(a_in), Box::pin(a_out));
        let conn_b = Connection::from_message_streams(Box::pin(b_in), Box::pin(b_out));
        conn_b.register_handler("ping", Arc::new(Echo)).await;
        let _: serde_json::Value = conn_a.call("ping", json!({})).await.expect("roundtrip");
    }

    #[tokio::test]
    async fn new_streams_roundtrips_a_call_in_lines_mode() {
        // Two mpsc<Bytes> pairs cross-wired between A and B.
        let (a_to_b_tx, a_to_b_rx) = mpsc::channel::<Bytes>(64);
        let (b_to_a_tx, b_to_a_rx) = mpsc::channel::<Bytes>(64);
        let conn_a = Connection::new_streams(b_to_a_rx, a_to_b_tx, Mode::Lines);
        let conn_b = Connection::new_streams(a_to_b_rx, b_to_a_tx, Mode::Lines);
        conn_b.register_handler("ping", Arc::new(Echo)).await;
        let _: serde_json::Value = conn_a.call("ping", json!({})).await.expect("roundtrip");
    }

    #[tokio::test]
    async fn mode_alias_equals_connection_mode() {
        let a: Mode = Mode::Lines;
        let b: ConnectionMode = ConnectionMode::Lines;
        assert_eq!(a, b);
    }
}
