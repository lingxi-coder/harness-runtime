//! Reader/writer orchestration. The broker drains outbound messages from the
//! [`Router`] onto a `Sink<Bytes>`, and reads inbound bytes from a
//! `Stream<Bytes>`, decoding each frame with the configured codec and
//! dispatching:
//! - [`Message::Request`] → [`Dispatcher::dispatch`] → response is queued on
//!   the writer's internal responder channel
//! - [`Message::Response`] → [`Router::dispatch_response`] (wakes pending
//!   oneshot)
//! - [`Message::Notification`] → broadcast on the `notifications` channel
//!
//! The broker is generic over the codec because some transports use
//! pre-decoded `Message` streams (e.g. WebSocket text frames) versus raw byte
//! streams (stdio with LSP / NDJSON codecs). We accept `Stream<Bytes>` +
//! `Sink<Bytes>` here and run the codec inside the broker — callers can wrap
//! either raw transports or `Framed<R, C>`-pre-segmented streams uniformly.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::codec::{Decoder, Encoder};

use crate::inbound::Dispatcher;
use crate::messages::{Message, Notification};
use crate::router::{OutboundMessage, Router};

/// Default capacity for the notifications broadcast channel — enough headroom
/// to weather a burst of `notifications/progress` while a slow consumer drains.
pub const DEFAULT_NOTIFICATION_CAPACITY: usize = 256;

/// Broker-level errors.
#[derive(Debug, Error)]
pub enum BrokerError {
    /// Codec returned an error during decode or encode.
    #[error("codec error: {0}")]
    Codec(#[from] crate::codec::CodecError),
    /// I/O error from the underlying transport (stream or sink).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The reader or writer task panicked or was cancelled.
    #[error("broker task join error: {0}")]
    Join(String),
}

/// Handle to the spawned broker tasks. Use [`join`](Self::join) to wait for
/// graceful shutdown (when the peer closes its half of the connection) or
/// [`abort`](Self::abort) to force both halves to stop.
#[must_use = "BrokerHandle drops when out of scope; keep it alive to keep the broker running"]
pub struct BrokerHandle {
    reader: JoinHandle<Result<(), BrokerError>>,
    writer: JoinHandle<Result<(), BrokerError>>,
}

impl BrokerHandle {
    /// Whether either broker half has terminated.
    ///
    /// A finished reader means the peer disconnected or framing failed even
    /// when the underlying child process is still alive.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.reader.is_finished() || self.writer.is_finished()
    }

    /// Await both halves to a graceful stop. Returns each half's terminal
    /// result. Useful in tests; in production callers usually spawn the
    /// handle's `join()` future and propagate any error from either side.
    pub async fn join(self) -> (Result<(), BrokerError>, Result<(), BrokerError>) {
        let r = match self.reader.await {
            Ok(r) => r,
            Err(e) => Err(BrokerError::Join(e.to_string())),
        };
        let w = match self.writer.await {
            Ok(w) => w,
            Err(e) => Err(BrokerError::Join(e.to_string())),
        };
        (r, w)
    }

    /// Force both halves to abort. Pending oneshots in the router will resolve
    /// with [`crate::router::RouterError::WriterClosed`] once the outbound
    /// queue is dropped.
    pub fn abort(&self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Spawn the broker over a pre-decoded `Stream<Message>` + `Sink<Message>`
/// pair. Used by transports that already decode JSON text frames upstream of
/// the JSON-RPC layer (e.g. WebSocket text-frame messages).
///
/// Skips the codec entirely. The provided sink's error type is mapped to
/// `BrokerError::Join` for uniform shutdown semantics.
pub fn spawn_typed<S, K>(
    inbound: S,
    outbound: K,
    router: Router,
    dispatcher: Dispatcher,
    outbound_rx: mpsc::UnboundedReceiver<OutboundMessage>,
) -> (BrokerHandle, broadcast::Receiver<Notification>)
where
    S: futures::Stream<Item = Message> + Send + Unpin + 'static,
    K: futures::Sink<Message, Error = crate::connection::ConnectionError> + Send + Unpin + 'static,
{
    let (notif_tx, notif_rx) = broadcast::channel(DEFAULT_NOTIFICATION_CAPACITY);

    // Responses produced by the inbound `Dispatcher` are fed back to the
    // writer task through this internal channel.
    let (responder_tx, responder_rx) = mpsc::unbounded_channel::<Message>();

    let reader_router = router.clone();
    let reader_close = router.close_handle();
    let reader_notif = notif_tx.clone();
    let reader = tokio::spawn(async move {
        let result = typed_reader_loop(
            inbound,
            reader_router,
            dispatcher,
            reader_notif,
            responder_tx,
        )
        .await;
        reader_close.close();
        result
    });

    let writer_close = router.close_handle();
    drop(router);
    let writer = tokio::spawn(async move {
        let result = typed_writer_loop(outbound, outbound_rx, responder_rx).await;
        writer_close.close();
        result
    });

    drop(notif_tx);

    (BrokerHandle { reader, writer }, notif_rx)
}

/// Reader half for `spawn_typed` — same routing logic as `reader_loop`, but
/// over pre-decoded `Message`s.
async fn typed_reader_loop<S>(
    mut inbound: S,
    router: Router,
    dispatcher: Dispatcher,
    notif_tx: broadcast::Sender<Notification>,
    responder_tx: mpsc::UnboundedSender<Message>,
) -> Result<(), BrokerError>
where
    S: futures::Stream<Item = Message> + Send + Unpin + 'static,
{
    while let Some(frame) = inbound.next().await {
        match frame {
            Message::Request(req) => {
                let dispatcher_clone = dispatcher.clone();
                let responder = responder_tx.clone();
                tokio::spawn(async move {
                    let resp = dispatcher_clone.dispatch(req).await;
                    let _ = responder.send(Message::Response(resp));
                });
            }
            Message::Response(resp) => {
                router.dispatch_response(resp);
            }
            Message::Notification(n) => {
                let _ = notif_tx.send(n);
            }
        }
    }
    Ok(())
}

/// Writer half for `spawn_typed` — drains the same two queues as
/// `writer_loop`, sending pre-decoded `Message`s without a codec layer.
async fn typed_writer_loop<K>(
    mut outbound: K,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundMessage>,
    mut responder_rx: mpsc::UnboundedReceiver<Message>,
) -> Result<(), BrokerError>
where
    K: futures::Sink<Message, Error = crate::connection::ConnectionError> + Send + Unpin + 'static,
{
    loop {
        let frame: Message = tokio::select! {
            biased;
            msg = outbound_rx.recv() => match msg {
                Some(OutboundMessage::Request(r)) => Message::Request(r),
                Some(OutboundMessage::Notification(n)) => Message::Notification(n),
                None => match responder_rx.recv().await {
                    Some(m) => m,
                    None => break,
                },
            },
            resp = responder_rx.recv() => match resp {
                Some(m) => m,
                None => match outbound_rx.recv().await {
                    Some(OutboundMessage::Request(r)) => Message::Request(r),
                    Some(OutboundMessage::Notification(n)) => Message::Notification(n),
                    None => break,
                },
            },
        };

        outbound
            .send(frame)
            .await
            .map_err(|e| BrokerError::Join(e.to_string()))?;
    }
    Ok(())
}

/// Spawn the broker tasks.
///
/// Returns:
/// - a [`BrokerHandle`] for joining / aborting,
/// - a [`broadcast::Receiver`] of inbound [`Notification`]s for the consumer.
///
/// Arguments:
/// - `inbound_bytes`: a stream of decoded byte frames straight from the
///   transport (raw or `Framed`-pre-segmented — both work because we feed the
///   bytes into a [`BytesMut`] and run the codec's `Decoder` in a loop).
/// - `outbound_bytes`: the corresponding sink for writes.
/// - `codec`: stateful codec used to encode outbound messages and decode
///   inbound ones.
/// - `router`: outbound router; the broker calls
///   [`Router::dispatch_response`] when an inbound `Response` arrives.
/// - `dispatcher`: inbound dispatcher; the broker calls
///   [`Dispatcher::dispatch`] for every inbound `Request` and forwards the
///   produced `Response` to the writer.
/// - `outbound_rx`: the receiver end of the channel handed to
///   [`Router::new`]; the writer task drains it.
pub fn spawn<S, K, C>(
    inbound_bytes: S,
    outbound_bytes: K,
    codec: C,
    router: Router,
    dispatcher: Dispatcher,
    outbound_rx: mpsc::UnboundedReceiver<OutboundMessage>,
) -> (BrokerHandle, broadcast::Receiver<Notification>)
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
    K: futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
    C: Encoder<Message, Error = crate::codec::CodecError>
        + Decoder<Item = Message, Error = crate::codec::CodecError>
        + Send
        + 'static,
{
    let (notif_tx, notif_rx) = broadcast::channel(DEFAULT_NOTIFICATION_CAPACITY);
    let codec = Arc::new(tokio::sync::Mutex::new(codec));

    // Responses produced by the inbound `Dispatcher` are fed back to the
    // writer task through this internal channel. The reader task owns the
    // sender; the writer task owns the receiver. Both halves merge into the
    // single outbound sink via `tokio::select!`.
    let (responder_tx, responder_rx) = mpsc::unbounded_channel::<Message>();

    let reader_codec = codec.clone();
    let reader_router = router.clone();
    let reader_close = router.close_handle();
    let reader_notif = notif_tx.clone();
    let reader = tokio::spawn(async move {
        let result = reader_loop(
            inbound_bytes,
            reader_codec,
            reader_router,
            dispatcher,
            reader_notif,
            responder_tx,
        )
        .await;
        reader_close.close();
        result
    });

    let writer_codec = codec;
    let writer_close = router.close_handle();
    drop(router);
    let writer = tokio::spawn(async move {
        let result = writer_loop(outbound_bytes, writer_codec, outbound_rx, responder_rx).await;
        writer_close.close();
        result
    });

    // `notif_tx` is dropped at the end of this function; the reader holds its
    // own clone. When the reader exits the channel closes on the consumer
    // side, which is the expected EOF signal.
    drop(notif_tx);

    (BrokerHandle { reader, writer }, notif_rx)
}

/// The reader half: pulls byte chunks from the transport, accumulates them in
/// a [`BytesMut`], runs the codec in a loop to extract whole frames, and
/// routes each [`Message`] to its destination.
async fn reader_loop<S, C>(
    mut inbound_bytes: S,
    codec: Arc<tokio::sync::Mutex<C>>,
    router: Router,
    dispatcher: Dispatcher,
    notif_tx: broadcast::Sender<Notification>,
    responder_tx: mpsc::UnboundedSender<Message>,
) -> Result<(), BrokerError>
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
    C: Decoder<Item = Message, Error = crate::codec::CodecError> + Send + 'static,
{
    let mut buf = BytesMut::new();
    while let Some(chunk) = inbound_bytes.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        loop {
            let mut codec_guard = codec.lock().await;
            let frame = match codec_guard.decode(&mut buf) {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => return Err(BrokerError::Codec(e)),
            };
            drop(codec_guard);

            match frame {
                Message::Request(req) => {
                    // Dispatch on a detached task so a slow handler does not
                    // block subsequent inbound frames.
                    let dispatcher_clone = dispatcher.clone();
                    let responder = responder_tx.clone();
                    tokio::spawn(async move {
                        let resp = dispatcher_clone.dispatch(req).await;
                        // If the writer is gone, drop silently — the
                        // connection is closing anyway.
                        let _ = responder.send(Message::Response(resp));
                    });
                }
                Message::Response(resp) => {
                    router.dispatch_response(resp);
                }
                Message::Notification(n) => {
                    // `send` only errors when there are no active receivers,
                    // which is fine — the broker still forwards future
                    // notifications if a consumer subscribes later.
                    let _ = notif_tx.send(n);
                }
            }
        }
    }
    Ok(())
}

/// The writer half: drains two sources (outbound queue from the [`Router`]
/// and the internal responder queue from the reader's [`Dispatcher`] calls)
/// and writes encoded frames onto the sink.
async fn writer_loop<K, C>(
    mut outbound_bytes: K,
    codec: Arc<tokio::sync::Mutex<C>>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundMessage>,
    mut responder_rx: mpsc::UnboundedReceiver<Message>,
) -> Result<(), BrokerError>
where
    K: futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
    C: Encoder<Message, Error = crate::codec::CodecError> + Send + 'static,
{
    loop {
        let frame: Message = tokio::select! {
            // Outbound queue first — back-pressure from local code wins ties.
            biased;
            msg = outbound_rx.recv() => match msg {
                Some(OutboundMessage::Request(r)) => Message::Request(r),
                Some(OutboundMessage::Notification(n)) => Message::Notification(n),
                // Outbound queue closed: the `Router` was dropped. Continue
                // draining inbound responses until that channel closes too.
                None => match responder_rx.recv().await {
                    Some(m) => m,
                    None => break,
                },
            },
            resp = responder_rx.recv() => match resp {
                Some(m) => m,
                // Responder closed: the reader task exited (peer disconnect).
                // Continue draining outbound until *that* channel also closes,
                // so any in-flight local notifications still get a chance to
                // make it onto the wire before we shut down.
                None => match outbound_rx.recv().await {
                    Some(OutboundMessage::Request(r)) => Message::Request(r),
                    Some(OutboundMessage::Notification(n)) => Message::Notification(n),
                    None => break,
                },
            },
        };

        let mut encoded = BytesMut::new();
        {
            let mut codec_guard = codec.lock().await;
            codec_guard.encode(frame, &mut encoded)?;
        }
        outbound_bytes.send(encoded.freeze()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineCodec;
    use crate::messages::{Id, Request, Response};
    use async_trait::async_trait;
    use serde_json::json;
    use std::time::Duration;

    struct EchoHandler;

    #[async_trait]
    impl crate::inbound::InboundHandler for EchoHandler {
        async fn handle(&self, req: Request) -> Response {
            Response::success(req.id, req.params.unwrap_or(json!(null)))
        }
    }

    /// Build a duplex pair of byte streams using mpsc channels. Returns
    /// `(in_stream, out_sink, in_tx, out_rx)`:
    /// - `in_stream` is what the broker reads from (peer → broker bytes).
    /// - `out_sink`  is what the broker writes into (broker → peer bytes).
    /// - `in_tx`     lets the test feed peer bytes to the broker.
    /// - `out_rx`    lets the test observe what the broker emitted.
    #[allow(clippy::type_complexity)]
    fn mpsc_duplex() -> (
        impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin + 'static,
        impl futures::Sink<Bytes, Error = std::io::Error> + Send + Unpin + 'static,
        mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
        mpsc::UnboundedReceiver<Bytes>,
    ) {
        use futures::stream::unfold;
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();
        let in_stream = Box::pin(unfold(in_rx, |mut rx| async move {
            rx.recv().await.map(|v| (v, rx))
        }));

        let (out_tx, out_rx) = mpsc::unbounded_channel::<Bytes>();
        let out_sink = Box::pin(futures::sink::unfold(
            out_tx,
            |tx, item: Bytes| async move {
                tx.send(item)
                    .map_err(|_| std::io::Error::other("sink closed"))?;
                Ok::<_, std::io::Error>(tx)
            },
        ));

        (in_stream, out_sink, in_tx, out_rx)
    }

    #[tokio::test]
    async fn broker_writes_outbound_request_through_writer_sink() {
        let (in_stream, out_sink, _in_tx, mut out_rx) = mpsc_duplex();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx);
        let dispatcher = Dispatcher::new();

        let (_handle, _notif_rx) = spawn(
            in_stream,
            out_sink,
            LineCodec::default(),
            router.clone(),
            dispatcher,
            outbound_rx,
        );

        // Schedule one outbound call. It will time out (no peer), but we only
        // care that the writer encoded and emitted the request frame.
        let r2 = router.clone();
        let h = tokio::spawn(async move {
            let _ = r2
                .call_with_timeout::<_, serde_json::Value>(
                    "ping",
                    json!({}),
                    Duration::from_millis(200),
                )
                .await;
        });

        let frame = out_rx.recv().await.expect("writer sink got a frame");
        let s = std::str::from_utf8(&frame).unwrap();
        assert!(s.contains("\"method\":\"ping\""), "frame was: {s:?}");
        assert!(s.ends_with('\n'), "LineCodec should LF-terminate");
        h.abort();
    }

    #[tokio::test]
    async fn broker_routes_inbound_response_back_to_pending_call() {
        let (in_stream, out_sink, in_tx, mut out_rx) = mpsc_duplex();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx);
        let dispatcher = Dispatcher::new();
        let (_handle, _notif_rx) = spawn(
            in_stream,
            out_sink,
            LineCodec::default(),
            router.clone(),
            dispatcher,
            outbound_rx,
        );

        // Drive `call` from a task; it will block on the oneshot until the
        // peer's `Response` arrives via the inbound stream.
        let r2 = router.clone();
        let call_handle = tokio::spawn(async move {
            r2.call_with_timeout::<_, serde_json::Value>("ping", json!({}), Duration::from_secs(2))
                .await
        });

        // Wait until the broker writes the outbound request so we know id=1
        // is pending in the router's map.
        let frame = out_rx.recv().await.expect("outbound request frame");
        let s = std::str::from_utf8(&frame).unwrap();
        assert!(s.contains("\"method\":\"ping\""), "frame was: {s:?}");

        // Now feed the matching response back through the inbound stream.
        let resp_bytes = serde_json::to_vec(&Message::Response(Response::success(
            Id::Number(1),
            json!({"pong": true}),
        )))
        .unwrap();
        let mut bytes = BytesMut::from(&resp_bytes[..]);
        bytes.extend_from_slice(b"\n");
        in_tx.send(Ok(bytes.freeze())).unwrap();

        let result = call_handle.await.expect("task join").expect("call ok");
        assert_eq!(result, json!({"pong": true}));
    }

    #[tokio::test]
    async fn broker_dispatches_inbound_request_to_dispatcher_and_writes_response() {
        let (in_stream, out_sink, in_tx, mut out_rx) = mpsc_duplex();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx);
        let dispatcher = Dispatcher::new();
        dispatcher.register("echo", Arc::new(EchoHandler)).await;

        let (_handle, _notif_rx) = spawn(
            in_stream,
            out_sink,
            LineCodec::default(),
            router,
            dispatcher,
            outbound_rx,
        );

        let req_bytes = serde_json::to_vec(&Message::Request(Request::new(
            "echo",
            Some(json!({"x": 1})),
            Id::Number(7),
        )))
        .unwrap();
        let mut bytes = BytesMut::from(&req_bytes[..]);
        bytes.extend_from_slice(b"\n");
        in_tx.send(Ok(bytes.freeze())).unwrap();

        let frame = out_rx.recv().await.expect("response frame");
        let s = std::str::from_utf8(&frame).unwrap();
        assert!(s.contains("\"id\":7"), "frame was: {s:?}");
        assert!(s.contains("\"result\""), "frame was: {s:?}");
        assert!(s.contains("\"x\":1"), "frame was: {s:?}");
    }

    #[tokio::test]
    async fn broker_fanouts_inbound_notifications_to_broadcast_receiver() {
        let (in_stream, out_sink, in_tx, _out_rx) = mpsc_duplex();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx);
        let dispatcher = Dispatcher::new();
        let (_handle, mut notif_rx) = spawn(
            in_stream,
            out_sink,
            LineCodec::default(),
            router,
            dispatcher,
            outbound_rx,
        );

        let n_bytes = serde_json::to_vec(&Message::Notification(Notification::new(
            "evt",
            Some(json!({"a": 1})),
        )))
        .unwrap();
        let mut bytes = BytesMut::from(&n_bytes[..]);
        bytes.extend_from_slice(b"\n");
        in_tx.send(Ok(bytes.freeze())).unwrap();

        let n = notif_rx.recv().await.expect("notification");
        assert_eq!(n.method, "evt");
        assert_eq!(n.params, Some(json!({"a": 1})));
    }

    #[tokio::test]
    async fn broker_handle_join_returns_ok_when_both_sides_close_cleanly() {
        let (in_stream, out_sink, in_tx, _out_rx) = mpsc_duplex();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let router = Router::new(outbound_tx);
        let dispatcher = Dispatcher::new();
        let (handle, _notif_rx) = spawn(
            in_stream,
            out_sink,
            LineCodec::default(),
            router.clone(),
            dispatcher,
            outbound_rx,
        );

        // Close the inbound stream (peer EOF) and drop the router (no more
        // outbound) → both halves should finish without an error.
        drop(in_tx);
        drop(router);

        let (r, w) = handle.join().await;
        assert!(r.is_ok(), "reader: {r:?}");
        assert!(w.is_ok(), "writer: {w:?}");
    }
}
