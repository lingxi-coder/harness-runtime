//! `ComputerAccessBroker` — the bridge-server-side sibling of
//! [`crate::adapter::AdapterPermissionGate`] for the `computer` tool's `request_access`
//! prompt (see `permission::computer_access`'s own doc comment for why
//! this round-trip bypasses the generic `PermissionGate`/`AdapterPermissionGate`
//! path entirely: a per-app-checkbox + tier + independent-capability-flags
//! prompt cannot be expressed by the generic title/message/options-list
//! `PermissionRequest` shape).
//!
//! ## Shape (mirrors `AdapterPermissionGate`, but channel-driven rather than
//! called synchronously)
//!
//! `AdapterPermissionGate` is *called* by `PermissionGate::check` on the engine
//! turn task. The `computer` tool's `request_access` instead already has its own
//! resolver seam (`tool_computer_use::access_resolver::ComputerAccessResolver`),
//! and `harness_runtime::desktop::build` already wires the GENERIC
//! `tool_computer_use::TuiBridgeResolver` (despite its name, just a channel
//! sender + oneshot awaiter — see that type's doc comment) onto
//! `DesktopConfig::computer_access_tx` whenever it is `Some`. So this broker
//! does not need its own resolver: it owns the RECEIVING end of that SAME
//! channel and:
//!
//! 1. [`ComputerAccessBroker::run`] drains
//!    `mpsc::Receiver<ComputerAccessExchange>` — one item per `request_access`
//!    call the `TuiBridgeResolver` sent. For each: reserve a fresh
//!    `request_id` (`AtomicU64`), park the exchange's own
//!    `oneshot::Sender<ComputerAccessResponse>` in the id-keyed map, lower the
//!    request to a [`ComputerAccessRequestDto`], and push it out through the
//!    connection's [`ComputerAccessRequestSink`].
//! 2. [`ComputerAccessBroker::resolve`]/[`ComputerAccessBroker::deny`]
//!    are called by the transport from a DIFFERENT task on an inbound
//!    `ApproveComputerAccess`/`DenyComputerAccess` command: they look up the id,
//!    remove the parked sender, and send the (converted, or fully-denied-default
//!    for `deny`) [`ComputerAccessResponse`] — which resolves the ORIGINAL
//!    `TuiBridgeResolver::resolve` await on the engine's tool-dispatch task.
//!
//! ## Fail-closed on disconnect
//!
//! [`ComputerAccessBroker::drain`] drops every parked sender: a dropped
//! `oneshot::Sender` resolves its receiver to `Err`, which
//! `TuiBridgeResolver::resolve` already maps to
//! `ComputerAccessResponse::default()` (fully denied) — the SAME fail-closed
//! guarantee `AdapterPermissionGate::drain` gives the generic permission path.
//! Call this alongside `AdapterPermissionGate::drain` on transport teardown.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::protocol::computer_access::{
    AccessTierDto, ComputerAccessRequestDto, ComputerAccessResponseDto, RequestedAppDto,
    TccStateDto,
};
use async_trait::async_trait;
use permission::computer_access::{
    AccessTier, ComputerAccessExchange, ComputerAccessResponse, RequestedApp, TccState,
};
use tokio::sync::{mpsc, oneshot, Mutex};

/// Transport-supplied destination for the outbound [`ComputerAccessRequestDto`].
///
/// Object-safe, mirroring [`crate::adapter::PermissionRequestSink`]: the transport wraps
/// each push into a `Frame::ComputerAccessRequest` (bridge-server) or the
/// matching mobile listener callback.
#[async_trait]
pub trait ComputerAccessRequestSink: Send + Sync {
    /// Forward one fully-lowered computer-access request to the underlying
    /// transport. Implementations should be cheap / non-blocking.
    async fn emit_request(&self, request: ComputerAccessRequestDto);
}

/// Lower `tui_core`'s [`AccessTier`] to the wire [`AccessTierDto`].
fn lower_tier(tier: AccessTier) -> AccessTierDto {
    match tier {
        AccessTier::Read => AccessTierDto::Read,
        AccessTier::Click => AccessTierDto::Click,
        AccessTier::Full => AccessTierDto::Full,
    }
}

/// Lower `tui_core`'s [`TccState`] to the wire [`TccStateDto`].
fn lower_tcc(tcc: TccState) -> TccStateDto {
    TccStateDto {
        accessibility: tcc.accessibility,
        screen_recording: tcc.screen_recording,
    }
}

/// Lower `tui_core`'s [`RequestedApp`] to the wire [`RequestedAppDto`].
fn lower_app(app: RequestedApp) -> RequestedAppDto {
    RequestedAppDto { label: app.label }
}

/// Raise the wire [`ComputerAccessResponseDto`] back to `tui_core`'s
/// [`ComputerAccessResponse`] (the type the parked `oneshot::Sender` expects).
fn raise_response(response: ComputerAccessResponseDto) -> ComputerAccessResponse {
    ComputerAccessResponse {
        granted_apps: response.granted_apps,
        clipboard_read: response.clipboard_read,
        clipboard_write: response.clipboard_write,
        system_key_combos: response.system_key_combos,
    }
}

/// The connection-scoped broker driving the `computer`-tool `request_access`
/// round-trip over the bridge-server transport. See the module doc for the
/// full shape.
pub struct ComputerAccessBroker {
    /// Where outbound [`ComputerAccessRequestDto`]s go (the transport).
    sink: Arc<dyn ComputerAccessRequestSink>,
    /// Monotonic `request_id` source. Connection-scoped, mirroring
    /// [`crate::adapter::AdapterPermissionGate`]'s id counter.
    next_id: AtomicU64,
    /// Parked exchanges' reply channels, keyed by `request_id`. The fail-closed
    /// owner: draining this map (or dropping the broker) drops every sender,
    /// resolving every in-flight `TuiBridgeResolver::resolve` await to the
    /// fully-denied default.
    pending: Mutex<HashMap<u64, oneshot::Sender<ComputerAccessResponse>>>,
}

impl ComputerAccessBroker {
    /// Construct a broker pushing outbound requests through `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn ComputerAccessRequestSink>) -> Self {
        Self {
            sink,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Drive the receive loop: consumes every [`ComputerAccessExchange`] the
    /// (generic) `tool_computer_use::TuiBridgeResolver` sends after
    /// `harness_runtime::desktop::build` wires it onto `DesktopConfig::computer_access_tx`.
    /// Runs until `rx`'s sender side is dropped (i.e. never, for the process
    /// lifetime of a bound connection) — spawn this on its own task per
    /// connection, exactly like the connection's other background loops.
    pub async fn run(&self, mut rx: mpsc::Receiver<ComputerAccessExchange>) {
        while let Some(exchange) = rx.recv().await {
            let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let ComputerAccessExchange { request, resp_tx } = exchange;
            {
                self.pending.lock().await.insert(request_id, resp_tx);
            }
            let dto = ComputerAccessRequestDto {
                request_id,
                reason: request.reason,
                apps: request.apps.into_iter().map(lower_app).collect(),
                tier: lower_tier(request.tier),
                clipboard_read: request.clipboard_read,
                clipboard_write: request.clipboard_write,
                system_key_combos: request.system_key_combos,
                tcc_state: request.tcc_state.map(lower_tcc),
            };
            self.sink.emit_request(dto).await;
        }
    }

    /// Resolve a parked request with the user's grant (an inbound
    /// `ApproveComputerAccess`). Returns `true` if a matching request was found
    /// and resolved, `false` if the id was unknown / already resolved (a safe
    /// no-op, mirroring [`crate::adapter::AdapterPermissionGate::resolve`]).
    pub async fn resolve(&self, request_id: u64, response: ComputerAccessResponseDto) -> bool {
        let Some(sender) = self.pending.lock().await.remove(&request_id) else {
            return false;
        };
        // If the receiver vanished (e.g. a drain raced this resolve), the send
        // simply fails; that path already resolved fully-denied, so it is safe
        // to ignore.
        sender.send(raise_response(response)).is_ok()
    }

    /// Deny a parked request (an inbound `DenyComputerAccess`) — resolves the
    /// awaiting `TuiBridgeResolver::resolve` call to the fully-denied default.
    /// Returns `true` if a matching request was found and resolved.
    pub async fn deny(&self, request_id: u64) -> bool {
        let Some(sender) = self.pending.lock().await.remove(&request_id) else {
            return false;
        };
        sender.send(ComputerAccessResponse::default()).is_ok()
    }

    /// Remove only requests whose owning tool has dropped its reply receiver.
    /// A new Agent owner or a background workflow keeps its live request.
    pub async fn cancel_closed(&self) -> usize {
        let mut pending = self.pending.lock().await;
        let before = pending.len();
        pending.retain(|_, reply| !reply.is_closed());
        before - pending.len()
    }

    /// Fail-closed drain: drop every parked sender so all in-flight
    /// `TuiBridgeResolver::resolve` awaits resolve to the fully-denied default
    /// (a dropped `oneshot::Sender` resolves its receiver to `Err`, which
    /// `TuiBridgeResolver::resolve` already maps to
    /// `ComputerAccessResponse::default()`). Call alongside
    /// `AdapterPermissionGate::drain` on transport teardown. Returns the
    /// number of requests that were drained.
    pub async fn drain(&self) -> usize {
        let mut pending = self.pending.lock().await;
        let n = pending.len();
        pending.clear();
        n
    }

    /// Number of requests currently parked (test/inspection helper).
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use permission::computer_access::{ComputerAccessRequest, TccState};
    use tokio::sync::Mutex as TokioMutex;

    /// A [`ComputerAccessRequestSink`] that captures every emitted request so a
    /// test can read back the assigned `request_id` and the lowered fields.
    #[derive(Default)]
    struct MockSink {
        requests: TokioMutex<Vec<ComputerAccessRequestDto>>,
    }

    impl MockSink {
        async fn requests(&self) -> Vec<ComputerAccessRequestDto> {
            self.requests.lock().await.clone()
        }
        async fn last(&self) -> ComputerAccessRequestDto {
            self.requests
                .lock()
                .await
                .last()
                .cloned()
                .expect("a request was emitted")
        }
    }

    #[async_trait]
    impl ComputerAccessRequestSink for MockSink {
        async fn emit_request(&self, request: ComputerAccessRequestDto) {
            self.requests.lock().await.push(request);
        }
    }

    #[tokio::test]
    async fn cancel_closed_preserves_another_owners_live_access_request() {
        let broker = ComputerAccessBroker::new(Arc::new(MockSink::default()));
        let (old_tx, old_rx) = oneshot::channel();
        let (live_tx, live_rx) = oneshot::channel();
        broker.pending.lock().await.insert(1, old_tx);
        broker.pending.lock().await.insert(2, live_tx);
        drop(old_rx);
        assert_eq!(broker.cancel_closed().await, 1);
        assert_eq!(broker.pending_count().await, 1);
        assert!(!broker.deny(1).await);
        assert!(broker.deny(2).await);
        let denied = live_rx.await.unwrap();
        assert!(denied.granted_apps.is_empty());
        assert!(!denied.clipboard_read && !denied.clipboard_write && !denied.system_key_combos);
    }

    fn sample_request() -> ComputerAccessRequest {
        ComputerAccessRequest {
            reason: "automate chat".to_string(),
            apps: vec![
                RequestedApp {
                    label: "Slack".to_string(),
                },
                RequestedApp {
                    label: "Chrome".to_string(),
                },
            ],
            tier: AccessTier::Full,
            clipboard_read: false,
            clipboard_write: false,
            system_key_combos: false,
            tcc_state: Some(TccState {
                accessibility: true,
                screen_recording: false,
            }),
        }
    }

    /// The broker lowers a [`ComputerAccessExchange`] to the wire DTO, assigns
    /// a fresh `request_id`, and `resolve()` sends the converted grant back on
    /// the exchange's own `resp_tx`.
    #[tokio::test]
    async fn run_lowers_request_and_resolve_grants() {
        let sink = std::sync::Arc::new(MockSink::default());
        let broker = std::sync::Arc::new(ComputerAccessBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(4);

        let broker_run = broker.clone();
        let run_task = tokio::spawn(async move { broker_run.run(rx).await });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(ComputerAccessExchange {
            request: sample_request(),
            resp_tx,
        })
        .await
        .unwrap();

        // Wait for the broker to have parked (and emitted) the request.
        for _ in 0..2000 {
            if broker.pending_count().await == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(broker.pending_count().await, 1);

        let dto = sink.last().await;
        assert_eq!(dto.reason, "automate chat");
        assert_eq!(dto.apps.len(), 2);
        assert_eq!(dto.tier, AccessTierDto::Full);
        assert_eq!(
            dto.tcc_state,
            Some(TccStateDto {
                accessibility: true,
                screen_recording: false
            })
        );

        assert!(
            broker
                .resolve(
                    dto.request_id,
                    ComputerAccessResponseDto {
                        granted_apps: vec!["Slack".to_string()],
                        clipboard_read: false,
                        clipboard_write: false,
                        system_key_combos: false,
                    },
                )
                .await
        );

        let response = resp_rx.await.expect("resp_tx was sent");
        assert_eq!(response.granted_apps, vec!["Slack".to_string()]);
        assert_eq!(broker.pending_count().await, 0);

        drop(tx);
        run_task.await.unwrap();
    }

    /// `deny()` resolves the parked exchange to the fully-denied default.
    #[tokio::test]
    async fn deny_resolves_fully_denied_default() {
        let sink = std::sync::Arc::new(MockSink::default());
        let broker = std::sync::Arc::new(ComputerAccessBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(4);
        let broker_run = broker.clone();
        let run_task = tokio::spawn(async move { broker_run.run(rx).await });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(ComputerAccessExchange {
            request: sample_request(),
            resp_tx,
        })
        .await
        .unwrap();
        for _ in 0..2000 {
            if broker.pending_count().await == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let dto = sink.last().await;

        assert!(broker.deny(dto.request_id).await);
        let response = resp_rx.await.expect("resp_tx was sent");
        assert!(response.granted_apps.is_empty());
        assert!(!response.clipboard_read);

        drop(tx);
        run_task.await.unwrap();
    }

    /// `drain()` drops every parked sender, which resolves the awaiting
    /// `resp_rx` to `Err` — the SAME signal `TuiBridgeResolver::resolve` already
    /// maps to a fully-denied default.
    #[tokio::test]
    async fn drain_drops_parked_senders() {
        let sink = std::sync::Arc::new(MockSink::default());
        let broker = std::sync::Arc::new(ComputerAccessBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(4);
        let broker_run = broker.clone();
        let run_task = tokio::spawn(async move { broker_run.run(rx).await });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(ComputerAccessExchange {
            request: sample_request(),
            resp_tx,
        })
        .await
        .unwrap();
        for _ in 0..2000 {
            if broker.pending_count().await == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let drained = broker.drain().await;
        assert_eq!(drained, 1);
        assert!(resp_rx.await.is_err(), "dropped sender resolves to Err");

        drop(tx);
        run_task.await.unwrap();
    }

    /// Resolving/denying an unknown id is a safe no-op.
    #[tokio::test]
    async fn resolve_and_deny_unknown_id_are_noop() {
        let sink = std::sync::Arc::new(MockSink::default());
        let broker = ComputerAccessBroker::new(sink);
        assert!(
            !broker
                .resolve(999, ComputerAccessResponseDto::default())
                .await
        );
        assert!(!broker.deny(999).await);
    }

    /// Concurrent exchanges get distinct ids and resolve independently.
    #[tokio::test]
    async fn concurrent_requests_get_distinct_ids() {
        let sink = std::sync::Arc::new(MockSink::default());
        let broker = std::sync::Arc::new(ComputerAccessBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(4);
        let broker_run = broker.clone();
        let run_task = tokio::spawn(async move { broker_run.run(rx).await });

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        tx.send(ComputerAccessExchange {
            request: sample_request(),
            resp_tx: tx1,
        })
        .await
        .unwrap();
        tx.send(ComputerAccessExchange {
            request: sample_request(),
            resp_tx: tx2,
        })
        .await
        .unwrap();
        for _ in 0..2000 {
            if broker.pending_count().await == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let reqs = sink.requests().await;
        assert_eq!(reqs.len(), 2);
        assert_ne!(reqs[0].request_id, reqs[1].request_id);

        assert!(
            broker
                .resolve(reqs[0].request_id, ComputerAccessResponseDto::default())
                .await
        );
        assert!(broker.deny(reqs[1].request_id).await);
        assert!(rx1.await.is_ok());
        assert!(rx2.await.is_ok());

        drop(tx);
        run_task.await.unwrap();
    }
}
