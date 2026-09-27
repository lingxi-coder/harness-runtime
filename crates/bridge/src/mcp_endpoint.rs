//! MCP-over-WebSocket endpoint exposed by the bridge.
//!
//! Clients connect at `ws://<host>:<port>/mcp` and must present the matching
//! `X-LingXi-Ide-Authorization` header (value = lockfile `authToken`).
//! Mismatched or missing tokens are rejected with HTTP 401 BEFORE the upgrade
//! completes (the response body is literally `"unauthorized\n"`).
//!
//! The literal header name and `mcp` subprotocol mirror the client side in
//! `lingxi-platform-common::mcp_ws` and `claude-code/src/services/mcp/client.ts`.

use crate::wire::Frame;
use futures_util::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;

/// Header name (canonical case) for the IDE auth token — matches the literal
/// `X-LingXi-Ide-Authorization` claude-code clients send.
/// `http::HeaderMap::get` is case-insensitive so the lookup works for any
/// case the client uses (claude-code's TS client lower-cases it).
pub const AUTH_HEADER_NAME: &str = "X-LingXi-Ide-Authorization";

/// LITERAL WebSocket subprotocol — single value `mcp` — echoed back on
/// successful upgrade when the client requested it.
pub const WS_SUBPROTOCOL: &str = "mcp";

/// Body of the 401 response sent on missing / mismatched auth — exact bytes
/// pinned by [`mcp_endpoint_test.rs`].
const UNAUTHORIZED_BODY: &str = "unauthorized\n";

/// How long the accept loop waits for in-flight connection tasks to close
/// their pump hooks before aborting them. An OAuth callback or a plugin
/// install runs inside one of those, so the budget is generous -- but host
/// shutdown awaits this, so it is a budget rather than an open wait.
const CONNECTION_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// Outer bound on joining the accept loop itself.
const ENDPOINT_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Largest single inbound WebSocket frame this endpoint will read.
///
/// DECLARED, not inherited. tungstenite's `WebSocketConfig::default()` already
/// caps a frame at 16 MiB, but that is a dependency's number: nothing here
/// stated it, a version bump could move it, and a client choosing its own
/// payload bounds had nothing on this side to read. It chose 24 MiB, which sat
/// above the real limit, and the consequence of the gap was not a rejected
/// command — an over-length frame makes the read yield
/// `Err(Capacity(MessageTooLong))`, which ends [`run_frame_pump`] and runs
/// `on_close_with_sink`, so `BridgeConnection::close_connection` aborts the
/// active turn and drains every broker. The user loses the session.
///
/// `MAX_BRIDGE_FRAME_BYTES` in `clients/shared/src/protocol.ts` is this number
/// on the client side, and `clients/electron/test/audio-engine-bounds.test.ts`
/// reads THIS declaration to prove the two still agree — which is only possible
/// because the value is written here rather than left to a default.
const MAX_INBOUND_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The WebSocket configuration every accepted connection is given.
///
/// `max_message_size` is pinned to the same value on purpose: nothing in this
/// protocol fragments a command across frames, so a message larger than one
/// frame is not something a well-formed client produces.
fn websocket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_frame_size: Some(MAX_INBOUND_FRAME_BYTES),
        max_message_size: Some(MAX_INBOUND_FRAME_BYTES),
        ..Default::default()
    }
}

/// A connection-scoped outbound sink the [`FramePump`] uses to push [`Frame`]s
/// back to the client — both the synchronous reply to an inbound command AND
/// later UNSOLICITED server pushes (streamed turn events, permission requests).
///
/// Cloneable: a pump can keep a clone alive past `on_frame` returning (e.g. a
/// spawned turn task that streams `Frame::Event`s) — sends keep flowing until
/// every clone is dropped or the client disconnects. Sending after the
/// connection has closed is a silent no-op (the write task is gone); the pump
/// does not need to handle that error.
#[derive(Clone)]
pub struct FrameSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl FrameSink {
    /// Queue `frame` for delivery to the client. Returns `true` if it was
    /// accepted into the outbound buffer, `false` if the connection's write
    /// task has already ended (the frame is dropped). Never blocks.
    ///
    /// The result is informational: fire-and-forget pushes may discard it
    /// (`let _ = sink.send(..)`) since a closed connection is not an error the
    /// pump must handle.
    #[must_use]
    pub fn send(&self, frame: Frame) -> bool {
        self.tx.send(frame).is_ok()
    }

    /// Whether both handles belong to the same WebSocket connection.
    ///
    /// This exposes channel identity without exposing the channel itself, so a
    /// shared pump can enforce a single active client.
    #[must_use]
    pub fn same_channel(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
}

/// A per-connection read/write pump callback supplied by the caller
/// (bridge-server). It is the seam between the proven transport (auth + upgrade
/// + framing, owned by [`McpEndpoint`]) and the engine routing (owned by the
/// caller, F2-05+).
///
/// For each inbound text frame the endpoint deserializes a [`Frame`] and calls
/// [`on_frame`](FramePump::on_frame), handing it the connection's [`FrameSink`]
/// for replies/pushes. Ordinary callbacks execute in receive order alongside
/// socket I/O; explicitly classified cancellation and interaction replies can
/// bypass a pending callback. One pump instance is shared across all
/// connections (`Arc<dyn FramePump>`); per-connection state lives behind the
/// `&self`/`FrameSink` boundary.
#[async_trait::async_trait]
pub trait FramePump: Send + Sync + 'static {
    /// Handle one inbound [`Frame`]. Use `out` to send reply / push frames.
    /// Endpoint shutdown waits for an accepted frame to finish before calling
    /// [`Self::on_close_with_sink`]. This future can include a multi-append
    /// durable command (for example compact boundary followed by summary), so
    /// dropping it on shutdown is unsafe. Long-running commands should support
    /// cooperative cancellation or own their work in the host lifecycle.
    async fn on_frame(&self, frame: Frame, out: FrameSink);

    /// Whether this frame may bypass a pending ordinary command. Authentication
    /// and connection ownership must still be checked by `on_frame`.
    fn is_priority_frame(&self, _frame: &Frame) -> bool {
        false
    }

    /// Called exactly once when the connection ends — the client disconnected,
    /// sent a Close, or the read side errored. The default is a no-op so
    /// hold-open pumps (e.g. the `EchoPump` test fixture) need no override.
    ///
    /// The bridge-server connection pump (F2-06) overrides this to DRAIN its
    /// `AdapterPermissionGate` (fail-closed): any in-flight permission `check()`
    /// parked on a oneshot resolves `Deny` the moment the resolving transport
    /// task vanishes, so a turn future can never hang waiting for an approval
    /// from a client that is gone.
    async fn on_close(&self) {}

    /// Connection-aware close notification. The default preserves the original
    /// [`Self::on_close`] API for pumps that do not need connection identity.
    async fn on_close_with_sink(&self, _sink: FrameSink) {
        self.on_close().await;
    }
}

/// Endpoint handle. Holds the listener port and the auth-token cell.
///
/// The accept loop runs in a background task spawned by
/// [`McpEndpoint::start_on_ephemeral_port`]; [`shutdown`](Self::shutdown)
/// signals that task to stop and waits for every accepted connection to run
/// its `FramePump::on_close_with_sink` teardown.
pub struct McpEndpoint {
    port: u16,
    auth_token: Arc<RwLock<Option<String>>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

impl McpEndpoint {
    /// Bind `127.0.0.1:0` and start accepting connections in a background task.
    ///
    /// # Errors
    /// Returns the I/O error from `TcpListener::bind` if the loopback port
    /// cannot be reserved (extremely rare; usually only when 127.0.0.1 itself
    /// is unreachable).
    pub async fn start_on_ephemeral_port() -> std::io::Result<Self> {
        Self::start_inner(None).await
    }

    /// Like [`start_on_ephemeral_port`](Self::start_on_ephemeral_port) but
    /// installs a [`FramePump`] driving each connection's read/write loop.
    ///
    /// The auth + upgrade + subprotocol handshake is IDENTICAL — only the
    /// post-upgrade behavior differs: with a pump, inbound text frames are
    /// deserialized to [`Frame`] and handed to the pump (which streams
    /// outbound frames back through the connection's [`FrameSink`]); without a
    /// pump, the socket is just held open until the client disconnects.
    ///
    /// # Errors
    /// Same as [`start_on_ephemeral_port`](Self::start_on_ephemeral_port): the
    /// I/O error from `TcpListener::bind` if the loopback port cannot be bound.
    pub async fn start_on_ephemeral_port_with_pump(
        pump: Arc<dyn FramePump>,
    ) -> std::io::Result<Self> {
        Self::start_inner(Some(pump)).await
    }

    /// Shared accept-loop constructor. `pump` is threaded to every connection
    /// task; `None` reproduces the historical hold-open behavior.
    async fn start_inner(pump: Option<Arc<dyn FramePump>>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let auth_token: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let (connection_shutdown_tx, _) = watch::channel(false);

        let auth_for_task = auth_token.clone();
        let accept_task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        let _ = connection_shutdown_tx.send(true);
                        // An in-flight frame handler can be an OAuth browser
                        // flow or a plugin install: give those the budget to
                        // finish, then stop waiting. Host shutdown awaits this
                        // task, and the desktop host is itself on a clock.
                        let drained = tokio::time::timeout(
                            CONNECTION_DRAIN_BUDGET,
                            async {
                                while connections.join_next().await.is_some() {}
                            },
                        )
                        .await;
                        if drained.is_err() {
                            tracing::warn!(
                                "bridge connections did not close within the drain budget"
                            );
                            connections.abort_all();
                            while connections.join_next().await.is_some() {}
                        }
                        break;
                    }
                    completed = connections.join_next(), if !connections.is_empty() => {
                        if let Some(Err(error)) = completed {
                            tracing::warn!(%error, "bridge connection task failed");
                        }
                    }
                    accept = listener.accept() => {
                        let (stream, addr) = match accept {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(error = %e, "bridge accept error");
                                continue;
                            }
                        };
                        let auth_for_conn = auth_for_task.clone();
                        let pump_for_conn = pump.clone();
                        connections.spawn(handle_connection(
                            stream,
                            addr,
                            auth_for_conn,
                            pump_for_conn,
                            connection_shutdown_tx.subscribe(),
                        ));
                    }
                }
            }
        });

        Ok(Self {
            port,
            auth_token,
            shutdown_tx: Some(shutdown_tx),
            accept_task: Some(accept_task),
        })
    }

    /// Update the expected auth token. Called by the bridge after writing the
    /// lockfile so the same `authToken` is enforced. Sync because the cell is
    /// a `std::sync::RwLock` — never blocks the runtime.
    pub fn set_auth_token(&self, token: String) {
        if let Ok(mut guard) = self.auth_token.write() {
            *guard = Some(token);
        }
    }

    /// Currently bound port (ephemeral, assigned by the kernel at bind time).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting, signal every connection loop, and wait for their pump
    /// close hooks. Once this returns no bridge request can create new model,
    /// task, or persistence work.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.accept_task.take() {
            // The accept loop bounds its own connection drain; this outer
            // bound covers the loop itself so a wedged accept cannot park the
            // host. It is deliberately larger than the inner one.
            if tokio::time::timeout(ENDPOINT_SHUTDOWN_BUDGET, &mut { task })
                .await
                .is_err()
            {
                tracing::warn!("bridge endpoint did not stop within the shutdown budget");
            }
        }
    }
}

/// Per-connection task. Captures the expected token at handshake time, runs
/// `accept_hdr_async` with an auth-validating callback, and (on success)
/// either drives the [`FramePump`] read/write loop (when one was supplied) or
/// holds the upgraded WebSocket open until the client disconnects.
async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    auth: Arc<RwLock<Option<String>>>,
    pump: Option<Arc<dyn FramePump>>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Snapshot the expected token BEFORE upgrade so the synchronous callback
    // can compare without re-acquiring the lock.
    let expected = auth.read().ok().and_then(|g| g.clone());

    let cb = move |req: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
        // `HeaderMap::get` is case-insensitive — one lookup covers both
        // canonical and lower-cased header forms.
        let supplied = req
            .headers()
            .get(AUTH_HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let authorized = match (expected.as_ref(), supplied.as_ref()) {
            (Some(want), Some(given)) => constant_time_eq(want.as_bytes(), given.as_bytes()),
            _ => false,
        };

        if !authorized {
            // tungstenite 0.21's `ErrorResponse` is `http::Response<Option<String>>`;
            // build via the http::Response builder and convert StatusCode.
            let resp = tokio_tungstenite::tungstenite::http::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("content-type", "text/plain; charset=utf-8")
                .body(Some(UNAUTHORIZED_BODY.to_string()))
                .expect("static 401 response must build");
            return Err(resp);
        }

        // Echo `Sec-WebSocket-Protocol: mcp` when the client requested it
        // (claude-code's TS client always does — see `protocols: ['mcp']`
        // in `src/services/mcp/client.ts`).
        if let Some(proto) = req.headers().get("sec-websocket-protocol") {
            if let Ok(p) = proto.to_str() {
                if p.split(',').any(|t| t.trim() == WS_SUBPROTOCOL) {
                    response.headers_mut().insert(
                        HeaderName::from_static("sec-websocket-protocol"),
                        HeaderValue::from_static(WS_SUBPROTOCOL),
                    );
                }
            }
        }
        Ok(response)
    };

    let upgraded = tokio::select! {
        biased;
        changed = shutdown.changed() => {
            let _ = changed;
            return;
        }
        upgraded = tokio_tungstenite::accept_hdr_async_with_config(
            stream,
            cb,
            Some(websocket_config()),
        ) => upgraded,
    };
    match upgraded {
        Ok(ws) => {
            tracing::debug!(?addr, "bridge: client connected");
            match pump {
                // With a pump: drive the real read/write frame loop.
                Some(pump) => run_frame_pump(ws, addr, pump, shutdown).await,
                // Without a pump: historical behavior — hold the socket open
                // until the client disconnects so the upgrade succeeds. (This
                // is the path `mcp_endpoint_test.rs`'s upgrade test exercises.)
                None => {
                    let _ = ws;
                }
            }
        }
        Err(e) => {
            tracing::debug!(?addr, error = %e, "bridge: handshake rejected");
        }
    }
}

/// Drive the post-upgrade read/write loop for a connection that has a
/// [`FramePump`].
///
/// Outbound frames flow through an unbounded mpsc channel ([`FrameSink`]) so
/// the pump can push from spawned tasks (e.g. a streamed turn) decoupled from
/// the read side. The loop ends when the client disconnects, sends a Close, or
/// every [`FrameSink`] clone is dropped AND the socket has no more inbound
/// frames.
async fn run_frame_pump<S>(
    ws: S,
    addr: SocketAddr,
    pump: Arc<dyn FramePump>,
    mut shutdown: watch::Receiver<bool>,
) where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Send
        + 'static,
{
    let (mut write, mut read) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let sink = FrameSink { tx: out_tx };

    // Run callbacks alongside socket I/O. Ordinary commands have one ordered
    // lane; only host-classified cancellation/interaction replies may bypass it.
    // Bound queued bytes as frames can individually contain large attachments.
    let mut ordinary: Option<BoxFuture<'static, ()>> = None;
    let mut priority: Option<BoxFuture<'static, ()>> = None;
    let mut ordinary_started = false;
    let mut priority_started = false;
    let mut queued = VecDeque::<(Frame, usize)>::new();
    let mut urgent = VecDeque::<(Frame, usize)>::new();
    let mut queued_bytes = 0usize;
    loop {
        for (active, started, queue) in [
            (&mut ordinary, &mut ordinary_started, &mut queued),
            (&mut priority, &mut priority_started, &mut urgent),
        ] {
            if active.is_none() {
                if let Some((frame, bytes)) = queue.pop_front() {
                    queued_bytes -= bytes;
                    let pump = pump.clone();
                    let sink = sink.clone();
                    *started = false;
                    *active = Some(async move { pump.on_frame(frame, sink).await }.boxed());
                }
            }
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                let _ = changed;
                tracing::debug!(?addr, "bridge: endpoint shutdown; ending pump");
                break;
            }
            () = async {
                priority_started = true;
                priority.as_mut().expect("guarded priority callback").await
            }, if priority.is_some() => {
                priority = None;
            }
            () = async {
                ordinary_started = true;
                ordinary.as_mut().expect("guarded ordinary callback").await
            }, if ordinary.is_some() => {
                ordinary = None;
            }
            // Outbound: a frame the pump queued → serialize + write. `recv`
            // only yields `None` once EVERY `FrameSink` clone is dropped; the
            // loop holds `sink` for its whole lifetime, so that arm is
            // effectively unreachable here and is treated as a no-op.
            maybe_out = out_rx.recv() => {
                if let Some(frame) = maybe_out {
                    match serde_json::to_string(&frame) {
                        Ok(text) => {
                            // Once an outer select arm is selected its body
                            // is no longer raced against shutdown. A peer that
                            // stops reading must not hold the endpoint's join
                            // drain (and the host lifecycle) indefinitely.
                            let sent = tokio::select! {
                                biased;
                                _ = shutdown.changed() => break,
                                sent = write.send(Message::Text(text)) => sent,
                            };
                            if sent.is_err() {
                                tracing::debug!(?addr, "bridge: write closed; ending pump");
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(?addr, error = %e, "bridge: outbound frame serialize failed");
                        }
                    }
                }
            }
            // Inbound: a message from the client.
            maybe_in = read.next() => {
                match maybe_in {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Frame>(&text) {
                            Ok(frame) => {
                                if queued.len() + urgent.len() >= 64
                                    || queued_bytes.saturating_add(text.len()) > 32 * 1024 * 1024
                                {
                                    tracing::warn!(?addr, "bridge: pending command limit exceeded");
                                    break;
                                }
                                queued_bytes += text.len();
                                if pump.is_priority_frame(&frame) {
                                    urgent.push_back((frame, text.len()));
                                } else {
                                    queued.push_back((frame, text.len()));
                                }
                            },
                            Err(e) => {
                                tracing::debug!(?addr, error = %e, "bridge: undecodable inbound frame ignored");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::debug!(?addr, "bridge: client disconnected");
                        break;
                    }
                    // Ping/Pong/Binary are not part of the JSON frame protocol;
                    // tungstenite auto-replies to Ping, so we just ignore them.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!(?addr, error = %e, "bridge: read error; ending pump");
                        break;
                    }
                }
            }
        }
    }

    // Never cancel a callback that may have started a durable transaction.
    // Queued callbacks have not been dispatched and are discarded like unread
    // socket frames. Promotion alone does not dispatch: shutdown can win before
    // the callback is first polled. Only finish actually started callbacks.
    tokio::join!(
        async {
            if ordinary_started {
                if let Some(callback) = ordinary {
                    callback.await;
                }
            }
        },
        async {
            if priority_started {
                if let Some(callback) = priority {
                    callback.await;
                }
            }
        },
    );

    // The connection ended (disconnect / Close / read error). Notify the pump so
    // it can run fail-closed teardown (the bridge-server connection drains its
    // permission gate here — F2-06).
    pump.on_close_with_sink(sink).await;
}

/// Constant-time byte-slice equality. Length-mismatch is short-circuited
/// (length is not secret). Used so an attacker timing the 401 path can't
/// recover the auth token byte-by-byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::Notify;

    /// One inbound command followed by a permanently stalled write flush.
    /// No socket-buffer sizing or sleeps are needed to force backpressure.
    struct BackpressuredSocket {
        inbound: Option<Message>,
        flushing: Arc<Notify>,
    }

    impl futures_util::Stream for BackpressuredSocket {
        type Item = Result<Message, tokio_tungstenite::tungstenite::Error>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.inbound.take() {
                Some(message) => Poll::Ready(Some(Ok(message))),
                None => Poll::Pending,
            }
        }
    }

    impl futures_util::Sink<Message> for BackpressuredSocket {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.flushing.notify_one();
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct ShutdownPump {
        frame_started: Notify,
        stall_frame: bool,
        finish_frame: Notify,
        closes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl FramePump for ShutdownPump {
        async fn on_frame(&self, frame: Frame, out: FrameSink) {
            self.frame_started.notify_one();
            if self.stall_frame {
                self.finish_frame.notified().await;
            }
            assert!(out.send(frame));
        }

        async fn on_close_with_sink(&self, _sink: FrameSink) {
            // The close hook itself must be awaited, not cancelled by the
            // already-ready shutdown notification.
            tokio::task::yield_now().await;
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn assert_shutdown_interrupts_pending_operation(stall_frame: bool) {
        let flushing = Arc::new(Notify::new());
        let frame = Frame::Request(crate::wire::BridgeRequest {
            id: 1,
            method: "test".into(),
            params: serde_json::Value::Null,
        });
        let socket = BackpressuredSocket {
            inbound: Some(Message::Text(serde_json::to_string(&frame).unwrap())),
            flushing: flushing.clone(),
        };
        let pump = Arc::new(ShutdownPump {
            frame_started: Notify::new(),
            stall_frame,
            finish_frame: Notify::new(),
            closes: AtomicUsize::new(0),
        });
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut task = tokio::spawn(run_frame_pump(
            socket,
            "127.0.0.1:1".parse().unwrap(),
            pump.clone(),
            shutdown_rx,
        ));
        let ready = async {
            if stall_frame {
                pump.frame_started.notified().await;
            } else {
                flushing.notified().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .expect("the selected operation must reach its pending point");
        shutdown_tx.send(true).unwrap();
        if stall_frame {
            // Accepted commands may have already committed part of a durable
            // transaction. Shutdown must wait for their remaining work, not
            // drop the future and mistake socket cleanup for command cleanup.
            assert!(tokio::time::timeout(Duration::from_millis(50), &mut task)
                .await
                .is_err());
            assert_eq!(pump.closes.load(Ordering::SeqCst), 0);
            pump.finish_frame.notify_one();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), &mut task).await;
        if result.is_err() {
            task.abort();
            let _ = task.await;
            panic!("shutdown must interrupt the pending operation and finish teardown");
        }
        result.unwrap().unwrap();
        assert_eq!(pump.closes.load(Ordering::SeqCst), 1);
    }

    struct LiveControlPump {
        started: Notify,
        release: Notify,
        seen: std::sync::Mutex<Vec<u64>>,
        shutdown_on_release: Option<watch::Sender<bool>>,
    }

    #[async_trait::async_trait]
    impl FramePump for LiveControlPump {
        fn is_priority_frame(&self, frame: &Frame) -> bool {
            matches!(frame, Frame::Request(request) if matches!(request.method.as_str(), "cancel" | "approve"))
        }
        async fn on_frame(&self, frame: Frame, out: FrameSink) {
            let Frame::Request(request) = &frame else {
                panic!("request")
            };
            self.seen.lock().unwrap().push(request.id);
            if request.id == 1 {
                assert!(out.send(frame.clone()));
                self.started.notify_one();
                self.release.notified().await;
                if let Some(shutdown) = &self.shutdown_on_release {
                    shutdown.send(true).unwrap();
                }
            } else {
                assert!(out.send(frame));
            }
        }
    }

    #[tokio::test]
    async fn slow_control_preserves_order_without_blocking_output_or_cancel() {
        let pump = Arc::new(LiveControlPump {
            started: Notify::new(),
            release: Notify::new(),
            seen: std::sync::Mutex::new(Vec::new()),
            shutdown_on_release: None,
        });
        let (client_io, server_io) = tokio::io::duplex(8192);
        let server = tokio_tungstenite::WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let mut client = tokio_tungstenite::WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_frame_pump(
            server,
            "127.0.0.1:1".parse().unwrap(),
            pump.clone(),
            shutdown_rx,
        ));
        for (id, method) in [(1, "model"), (2, "model"), (3, "cancel"), (4, "approve")] {
            let frame = Frame::Request(crate::wire::BridgeRequest {
                id,
                method: method.into(),
                params: serde_json::Value::Null,
            });
            client
                .send(Message::Text(serde_json::to_string(&frame).unwrap()))
                .await
                .unwrap();
        }
        pump.started.notified().await;
        let observed = tokio::time::timeout(Duration::from_secs(1), async {
            let mut ids = Vec::new();
            for _ in 0..3 {
                let message = client.next().await.unwrap().unwrap();
                let Frame::Request(request) =
                    serde_json::from_str::<Frame>(message.to_text().unwrap()).unwrap()
                else {
                    panic!("request")
                };
                ids.push(request.id);
            }
            ids
        })
        .await;
        let before_release = pump.seen.lock().unwrap().clone();
        pump.release.notify_one();
        if observed.is_ok() {
            let message = tokio::time::timeout(Duration::from_secs(1), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Frame::Request(request) =
                serde_json::from_str::<Frame>(message.to_text().unwrap()).unwrap()
            else {
                panic!("request")
            };
            assert_eq!(request.id, 2);
        }
        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(
            observed.expect("output and cancellation must bypass a slow control"),
            vec![1, 3, 4]
        );
        assert_eq!(
            before_release,
            vec![1, 3, 4],
            "ordinary controls stay ordered"
        );
    }

    #[tokio::test]
    async fn shutdown_does_not_start_a_promoted_but_unpolled_command() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let pump = Arc::new(LiveControlPump {
            started: Notify::new(),
            release: Notify::new(),
            seen: std::sync::Mutex::new(Vec::new()),
            shutdown_on_release: Some(shutdown_tx),
        });
        let (client_io, server_io) = tokio::io::duplex(8192);
        let server = tokio_tungstenite::WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let mut client = tokio_tungstenite::WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let task = tokio::spawn(run_frame_pump(
            server,
            "127.0.0.1:1".parse().unwrap(),
            pump.clone(),
            shutdown_rx,
        ));
        for (id, method) in [(1, "model"), (2, "model"), (3, "approve")] {
            let frame = Frame::Request(crate::wire::BridgeRequest {
                id,
                method: method.into(),
                params: serde_json::Value::Null,
            });
            client
                .send(Message::Text(serde_json::to_string(&frame).unwrap()))
                .await
                .unwrap();
        }
        // The priority echo proves B has already been read and queued. A
        // raises shutdown during its last poll, after select polled shutdown
        // as Pending. B will be promoted on the next loop but must not start.
        tokio::time::timeout(Duration::from_secs(1), async {
            for _ in 0..2 {
                client.next().await.unwrap().unwrap();
            }
        })
        .await
        .unwrap();
        pump.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*pump.seen.lock().unwrap(), vec![1, 3]);
    }

    #[tokio::test]
    async fn shutdown_interrupts_backpressured_write_and_awaits_close_once() {
        assert_shutdown_interrupts_pending_operation(false).await;
    }

    #[tokio::test]
    async fn shutdown_finishes_accepted_frame_before_closing_once() {
        assert_shutdown_interrupts_pending_operation(true).await;
    }

    #[test]
    fn websocket_config_declares_the_inbound_frame_limit() {
        // The constant is only worth pinning from the TypeScript side if the
        // endpoint actually runs with it; a decorative constant next to an
        // `accept_hdr_async` with no config would pass that pin and still
        // read whatever tungstenite defaults to.
        let config = websocket_config();
        assert_eq!(config.max_frame_size, Some(MAX_INBOUND_FRAME_BYTES));
        assert_eq!(config.max_message_size, Some(MAX_INBOUND_FRAME_BYTES));
    }

    #[test]
    fn constant_time_eq_matches_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn endpoint_binds_loopback_port() {
        let ep = McpEndpoint::start_on_ephemeral_port().await.unwrap();
        assert!(ep.port() > 0, "ephemeral port must be assigned");
        ep.shutdown().await;
    }

    #[tokio::test]
    async fn set_auth_token_is_observable() {
        let ep = McpEndpoint::start_on_ephemeral_port().await.unwrap();
        ep.set_auth_token("token-abc".to_string());
        let observed = ep.auth_token.read().unwrap().clone();
        assert_eq!(observed.as_deref(), Some("token-abc"));
        ep.shutdown().await;
    }
}
