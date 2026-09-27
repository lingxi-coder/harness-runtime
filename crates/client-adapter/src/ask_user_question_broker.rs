//! Connection-scoped `AskUserQuestion` request/response broker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use client_protocol::ask_user_question::{AskOptionDto, AskQuestionDto, AskUserQuestionRequestDto};
use client_protocol::events::ClientEvent;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use tool_api::ask_user_question::{AskOption, AskQuestion, AskUserQuestionExchange};

use crate::ClientEventSink;

const MAX_PENDING_ASK_USER_QUESTIONS: usize = 64;
const PUBLICATION_QUEUED: u8 = 0;
const PUBLICATION_IN_FLIGHT: u8 = 1;
const PUBLICATION_COMPLETE: u8 = 2;
const PUBLICATION_REMOVED: u8 = 3;

struct PendingAskUserQuestion {
    resp_tx: oneshot::Sender<HashMap<String, String>>,
    timeout_task: Option<JoinHandle<()>>,
    publication_state: AtomicU8,
    publication_complete: Arc<Notify>,
    /// The emitted request, retained verbatim so a reconnecting client can
    /// have every still-parked question replayed with its ORIGINAL (stable)
    /// `request_id` — the tool side keeps waiting on the same oneshot across
    /// the disconnect, so the correlator must not change either.
    request: AskUserQuestionRequestDto,
}

impl PendingAskUserQuestion {
    fn new(
        resp_tx: oneshot::Sender<HashMap<String, String>>,
        request: AskUserQuestionRequestDto,
    ) -> Self {
        Self {
            resp_tx,
            timeout_task: None,
            publication_state: AtomicU8::new(PUBLICATION_QUEUED),
            publication_complete: Arc::new(Notify::new()),
            request,
        }
    }

    fn claim_publication(&self) -> bool {
        self.publication_state
            .compare_exchange(
                PUBLICATION_QUEUED,
                PUBLICATION_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn claim_republication(&self) -> bool {
        self.publication_state
            .compare_exchange(
                PUBLICATION_COMPLETE,
                PUBLICATION_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn mark_publication_complete(&self) {
        self.publication_state
            .store(PUBLICATION_COMPLETE, Ordering::Release);
        self.publication_complete.notify_waiters();
    }

    fn mark_removed(&self) {
        self.publication_state
            .store(PUBLICATION_REMOVED, Ordering::Release);
        self.publication_complete.notify_waiters();
    }
}

fn lower_option(option: AskOption) -> AskOptionDto {
    AskOptionDto {
        label: option.label,
        description: option.description,
        preview: option.preview,
    }
}

fn lower_question(question: AskQuestion) -> AskQuestionDto {
    AskQuestionDto {
        question: question.question,
        header: question.header,
        options: question.options.into_iter().map(lower_option).collect(),
        multi_select: question.multi_select,
    }
}

/// Bridges the tool's session-scoped questionnaire channel to a client event
/// stream and correlates inbound answers back to the parked tool call.
///
/// Ids are connection-scoped `u64`s, but STABLE for the lifetime of the
/// parked tool call — [`Self::replay_pending`] re-announces a still-parked
/// request with its original id after a reconnect, and the tool side keeps
/// waiting on the same oneshot across the disconnect. Nothing here needs to
/// be durable: the parked turn dies with the process.
pub struct BridgeAskUserQuestionBroker {
    sink: Arc<dyn ClientEventSink>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, PendingAskUserQuestion>>>,
    #[cfg(test)]
    timeout_gate: Option<Arc<Notify>>,
}

impl BridgeAskUserQuestionBroker {
    /// Construct a connection-scoped broker.
    #[must_use]
    pub fn new(sink: Arc<dyn ClientEventSink>) -> Self {
        Self {
            sink,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            timeout_gate: None,
        }
    }

    /// Drain the tool-side exchange channel until its sender is dropped.
    pub async fn run(&self, mut rx: mpsc::Receiver<AskUserQuestionExchange>) {
        while let Some(exchange) = rx.recv().await {
            let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let AskUserQuestionExchange {
                questions,
                timeout_secs,
                resp_tx,
            } = exchange;
            let request = AskUserQuestionRequestDto {
                request_id,
                questions: questions.into_iter().map(lower_question).collect(),
                timeout_secs,
            };
            // The turn can be cancelled after enqueueing the exchange but
            // before this broker task gets scheduled. In that window the
            // resolver has already dropped its receiver, so never publish a
            // request that cannot be answered. Claim publication while still
            // holding the pending lock: cleanup can then either remove the
            // queued entry or wait for this publication to finish, but cannot
            // emit Resolved and let us publish Ask afterward.
            let should_publish = {
                let mut pending = self.pending.lock().await;
                if resp_tx.is_closed() {
                    continue;
                }
                if pending.len() >= MAX_PENDING_ASK_USER_QUESTIONS {
                    tracing::warn!(
                        limit = MAX_PENDING_ASK_USER_QUESTIONS,
                        "dropping AskUserQuestion request because the pending limit was reached"
                    );
                    continue;
                }
                pending.insert(
                    request_id,
                    PendingAskUserQuestion::new(resp_tx, request.clone()),
                );
                pending
                    .get(&request_id)
                    .is_some_and(PendingAskUserQuestion::claim_publication)
            };
            if !should_publish {
                if let Some(entry) = self.pending.lock().await.remove(&request_id) {
                    entry.mark_removed();
                }
                continue;
            }
            self.sink
                .emit(ClientEvent::AskUserQuestion { request })
                .await;
            if let Some(entry) = self.pending.lock().await.get(&request_id) {
                entry.mark_publication_complete();
            }
            if let Some(timeout_secs) = timeout_secs {
                let pending = Arc::clone(&self.pending);
                let sink = Arc::clone(&self.sink);
                #[cfg(test)]
                let timeout_gate = self.timeout_gate.clone();
                let timeout_task = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
                    #[cfg(test)]
                    if let Some(timeout_gate) = timeout_gate {
                        timeout_gate.notified().await;
                    }
                    let Some(entry) = BridgeAskUserQuestionBroker::take_after_publication_from(
                        &pending, request_id,
                    )
                    .await
                    else {
                        return;
                    };
                    // The client supplied no confirmed answers. Preserve that
                    // fact instead of converting the highlighted first row into
                    // a user decision.
                    let _ = entry.resp_tx.send(HashMap::new());
                    sink.emit(ClientEvent::AskUserQuestionResolved { request_id })
                        .await;
                });
                let mut pending = self.pending.lock().await;
                if let Some(entry) = pending.get_mut(&request_id) {
                    entry.timeout_task = Some(timeout_task);
                } else {
                    timeout_task.abort();
                }
            }
        }
    }

    /// Resolve one parked questionnaire. Unknown/already-resolved ids are safe
    /// no-ops and return `false`.
    pub async fn resolve(&self, request_id: u64, answers: HashMap<String, String>) -> bool {
        let Some(entry) = self.take_after_publication(request_id).await else {
            return false;
        };
        if let Some(task) = entry.timeout_task {
            task.abort();
        }
        let resolved = entry.resp_tx.send(answers).is_ok();
        self.sink
            .emit(ClientEvent::AskUserQuestionResolved { request_id })
            .await;
        resolved
    }

    /// Cancel one parked questionnaire by dropping its response sender.
    pub async fn cancel(&self, request_id: u64) -> bool {
        let Some(entry) = self.take_after_publication(request_id).await else {
            return false;
        };
        if let Some(task) = entry.timeout_task {
            task.abort();
        }
        drop(entry.resp_tx);
        self.sink
            .emit(ClientEvent::AskUserQuestionResolved { request_id })
            .await;
        true
    }

    /// Remove requests whose tool-side response receiver has been dropped.
    ///
    /// A live turn can be cooperatively quiesced while a Block-style question
    /// is parked. Its resolver future is dropped by the turn cancellation
    /// path, which closes only that request's sender. Workflow requests share
    /// this broker but keep their receivers open, so they are left untouched.
    pub async fn cancel_closed(&self) -> usize {
        let closed = loop {
            let (waiters, closed) = {
                let mut pending = self.pending.lock().await;
                let mut waiters = Vec::new();
                let mut closed_ids = Vec::new();
                for (request_id, entry) in pending.iter() {
                    if !entry.resp_tx.is_closed() {
                        continue;
                    }
                    if entry.publication_state.load(Ordering::Acquire) == PUBLICATION_IN_FLIGHT {
                        let mut waiter =
                            Box::pin(entry.publication_complete.clone().notified_owned());
                        // Register the waiter before releasing `pending` so a
                        // publisher cannot complete in the gap and lose the
                        // wakeup delivered by `notify_waiters`.
                        waiter.as_mut().enable();
                        waiters.push(waiter);
                    } else {
                        closed_ids.push(*request_id);
                    }
                }
                if waiters.is_empty() {
                    let closed = closed_ids
                        .into_iter()
                        .filter_map(|request_id| {
                            let entry = pending.remove(&request_id)?;
                            entry.mark_removed();
                            Some((request_id, entry))
                        })
                        .collect::<Vec<_>>();
                    (waiters, closed)
                } else {
                    (waiters, Vec::new())
                }
            };
            if waiters.is_empty() {
                break closed;
            }
            for waiter in waiters {
                waiter.await;
            }
        };
        let count = closed.len();
        for (request_id, entry) in closed {
            if let Some(task) = entry.timeout_task {
                task.abort();
            }
            drop(entry.resp_tx);
            self.sink
                .emit(ClientEvent::AskUserQuestionResolved { request_id })
                .await;
        }
        count
    }

    /// Remove one request only after its client-facing publication has either
    /// completed or never started. This keeps a fast answer/cancel from
    /// publishing `Resolved` ahead of the corresponding `AskUserQuestion`.
    async fn take_after_publication(&self, request_id: u64) -> Option<PendingAskUserQuestion> {
        Self::take_after_publication_from(&self.pending, request_id).await
    }

    async fn take_after_publication_from(
        pending: &Arc<Mutex<HashMap<u64, PendingAskUserQuestion>>>,
        request_id: u64,
    ) -> Option<PendingAskUserQuestion> {
        loop {
            let waiter = {
                let mut pending = pending.lock().await;
                let entry = pending.get(&request_id)?;
                if entry.publication_state.load(Ordering::Acquire) == PUBLICATION_IN_FLIGHT {
                    let mut waiter = Box::pin(entry.publication_complete.clone().notified_owned());
                    // Pair registration with the state observation while the
                    // map lock is held; completion cannot notify between the
                    // observation and waiter registration.
                    waiter.as_mut().enable();
                    waiter
                } else {
                    let entry = pending.remove(&request_id)?;
                    entry.mark_removed();
                    return Some(entry);
                }
            };
            waiter.await;
        }
    }

    /// Re-emit every still-parked request with its ORIGINAL id — the
    /// reconnect path. A mobile client that backgrounded (dropping the event
    /// stream while the engine process stayed alive) re-renders its pending
    /// question cards from this replay, the same shape as the app side's
    /// `resync_pending_gates`. Safe to call repeatedly; order is unspecified
    /// (at most `MAX_PENDING_ASK_USER_QUESTIONS` live at once, and each id
    /// appears at most once).
    pub async fn replay_pending(&self) -> usize {
        let requests: Vec<(u64, AskUserQuestionRequestDto)> = {
            let pending = &mut *self.pending.lock().await;
            pending
                .iter_mut()
                .filter_map(|(request_id, entry)| {
                    if entry.resp_tx.is_closed() || !entry.claim_republication() {
                        return None;
                    }
                    Some((*request_id, entry.request.clone()))
                })
                .collect()
        };
        let count = requests.len();
        for (request_id, request) in requests {
            self.sink
                .emit(ClientEvent::AskUserQuestion { request })
                .await;
            if let Some(entry) = self.pending.lock().await.get(&request_id) {
                entry.mark_publication_complete();
            }
        }
        count
    }

    /// Fail closed on disconnect/session teardown.
    pub async fn drain(&self) -> usize {
        let drained = loop {
            let (waiters, drained) = {
                let pending = self.pending.lock().await;
                let waiters = pending
                    .values()
                    .filter(|entry| {
                        entry.publication_state.load(Ordering::Acquire) == PUBLICATION_IN_FLIGHT
                    })
                    .map(|entry| {
                        let mut waiter =
                            Box::pin(entry.publication_complete.clone().notified_owned());
                        waiter.as_mut().enable();
                        waiter
                    })
                    .collect::<Vec<_>>();
                (waiters, pending.is_empty())
            };
            if !waiters.is_empty() {
                for waiter in waiters {
                    waiter.await;
                }
                continue;
            }
            if drained {
                break Vec::new();
            }
            let mut pending = self.pending.lock().await;
            if pending.values().any(|entry| {
                entry.publication_state.load(Ordering::Acquire) == PUBLICATION_IN_FLIGHT
            }) {
                continue;
            }
            let drained = pending
                .drain()
                .map(|(request_id, entry)| {
                    entry.mark_removed();
                    (request_id, entry)
                })
                .collect::<Vec<_>>();
            break drained;
        };
        let count = drained.len();
        for (request_id, entry) in drained {
            if let Some(task) = entry.timeout_task {
                task.abort();
            }
            // Drop the response sender before notifying the client so the
            // blocked tool is already guaranteed to unwind when the UI clears.
            drop(entry.resp_tx);
            self.sink
                .emit(ClientEvent::AskUserQuestionResolved { request_id })
                .await;
        }
        count
    }

    /// Number of currently parked requests.
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientEventSink, MockSink};
    use async_trait::async_trait;
    use tool_api::ask_user_question::{AskOption, AskQuestion};

    #[derive(Default)]
    struct BarrierSink {
        ask_entered: Notify,
        release_ask: Notify,
        events: Mutex<Vec<ClientEvent>>,
    }

    #[async_trait]
    impl ClientEventSink for BarrierSink {
        async fn emit(&self, event: ClientEvent) {
            if matches!(event, ClientEvent::AskUserQuestion { .. }) {
                self.ask_entered.notify_one();
                self.release_ask.notified().await;
            }
            self.events.lock().await.push(event);
        }
    }

    #[derive(Default)]
    struct ReplayBarrierSink {
        ask_count: AtomicU64,
        replay_entered: Notify,
        release_replay: Notify,
        events: Mutex<Vec<ClientEvent>>,
    }

    #[async_trait]
    impl ClientEventSink for ReplayBarrierSink {
        async fn emit(&self, event: ClientEvent) {
            if matches!(event, ClientEvent::AskUserQuestion { .. })
                && self.ask_count.fetch_add(1, Ordering::Relaxed) == 1
            {
                self.replay_entered.notify_one();
                self.release_replay.notified().await;
            }
            self.events.lock().await.push(event);
        }
    }

    fn an_exchange(resp_tx: oneshot::Sender<HashMap<String, String>>) -> AskUserQuestionExchange {
        AskUserQuestionExchange {
            questions: vec![AskQuestion {
                question: "Choose?".into(),
                header: "Choice".into(),
                options: vec![AskOption::new("A", "first")],
                multi_select: false,
            }],
            timeout_secs: None,
            resp_tx,
        }
    }

    async fn first_request_id(sink: &Arc<MockSink>) -> u64 {
        loop {
            if let Some(ClientEvent::AskUserQuestion { request }) =
                sink.events().await.into_iter().next()
            {
                break request.request_id;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn emits_and_resolves_correlated_questionnaire() {
        let sink = MockSink::arc();
        let broker = Arc::new(BridgeAskUserQuestionBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(1);
        let runner = broker.clone();
        let task = tokio::spawn(async move { runner.run(rx).await });
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(an_exchange(resp_tx)).await.unwrap();

        let request_id = first_request_id(&sink).await;
        assert!(
            broker
                .resolve(request_id, HashMap::from([("Choose?".into(), "A".into())]))
                .await
        );
        assert_eq!(resp_rx.await.unwrap()["Choose?"], "A");
        assert!(sink.events().await.into_iter().any(|event| matches!(
            event,
            ClientEvent::AskUserQuestionResolved { request_id: id } if id == request_id
        )));
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn delayed_broker_start_discards_exchange_closed_before_insertion() {
        let sink = MockSink::arc();
        let broker = BridgeAskUserQuestionBroker::new(sink.clone());
        let (tx, rx) = mpsc::channel(1);
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(an_exchange(resp_tx)).await.unwrap();
        // Model a turn cancellation before the broker task starts consuming
        // its queued exchange. The resolver's receiver is already gone by the
        // time `run` attempts insertion.
        drop(resp_rx);
        drop(tx);

        broker.run(rx).await;

        assert_eq!(broker.pending_count().await, 0);
        assert!(sink.events().await.is_empty());
    }

    #[tokio::test]
    async fn cleanup_waits_for_inflight_publication_before_resolved() {
        let sink = Arc::new(BarrierSink::default());
        let broker = Arc::new(BridgeAskUserQuestionBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(1);
        let runner = broker.clone();
        tokio::spawn(async move { runner.run(rx).await });
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(an_exchange(resp_tx)).await.unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sink.ask_entered.notified(),
        )
        .await
        .expect("broker entered Ask publication");
        drop(resp_rx);

        let mut cleanup = tokio::spawn({
            let broker = broker.clone();
            async move { broker.cancel_closed().await }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut cleanup)
                .await
                .is_err(),
            "cleanup must not publish Resolved while Ask publication is blocked"
        );
        sink.release_ask.notify_one();

        assert_eq!(cleanup.await.expect("cleanup joined"), 1);
        let events = sink.events.lock().await.clone();
        assert!(matches!(
            events.as_slice(),
            [
                ClientEvent::AskUserQuestion { .. },
                ClientEvent::AskUserQuestionResolved { .. }
            ]
        ));
        drop(tx);
    }

    #[tokio::test]
    async fn timeout_waits_for_blocked_replay_before_resolved() {
        let sink = Arc::new(ReplayBarrierSink::default());
        let timeout_gate = Arc::new(Notify::new());
        let mut broker_inner = BridgeAskUserQuestionBroker::new(sink.clone());
        broker_inner.timeout_gate = Some(timeout_gate.clone());
        let broker = Arc::new(broker_inner);
        let (tx, rx) = mpsc::channel(1);
        let runner = broker.clone();
        let task = tokio::spawn(async move { runner.run(rx).await });
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AskUserQuestionExchange {
            questions: vec![AskQuestion {
                question: "Choose?".into(),
                header: "Choice".into(),
                options: vec![AskOption::new("A", "first")],
                multi_select: false,
            }],
            timeout_secs: Some(0),
            resp_tx,
        })
        .await
        .unwrap();

        let request_id = loop {
            let events = sink.events.lock().await;
            if let Some(ClientEvent::AskUserQuestion { request }) = events.first() {
                break request.request_id;
            }
            drop(events);
            tokio::task::yield_now().await;
        };
        // Ensure the timeout task has been installed, then start replay before
        // releasing the timeout gate. The second Ask publication is blocked;
        // timeout must wait on its publication barrier instead of resolving
        // the entry ahead of the replayed request.
        loop {
            let pending = broker.pending.lock().await;
            if pending
                .get(&request_id)
                .and_then(|entry| entry.timeout_task.as_ref())
                .is_some()
            {
                break;
            }
            drop(pending);
            tokio::task::yield_now().await;
        }
        let replay = tokio::spawn({
            let broker = broker.clone();
            async move { broker.replay_pending().await }
        });
        tokio::time::timeout(Duration::from_secs(1), sink.replay_entered.notified())
            .await
            .expect("replay entered Ask publication");

        timeout_gate.notify_one();
        tokio::task::yield_now().await;
        let events = sink.events.lock().await.clone();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ClientEvent::AskUserQuestion { .. }));

        sink.release_replay.notify_one();
        assert_eq!(replay.await.expect("replay joined"), 1);
        assert!(resp_rx.await.unwrap().is_empty());
        assert_eq!(broker.pending_count().await, 0);
        let events = sink.events.lock().await.clone();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], ClientEvent::AskUserQuestion { .. }));
        assert!(matches!(
            &events[1],
            ClientEvent::AskUserQuestion { request } if request.request_id == request_id
        ));
        assert!(matches!(
            events[2],
            ClientEvent::AskUserQuestionResolved { request_id: id } if id == request_id
        ));
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn replay_re_emits_parked_requests_with_the_same_id() {
        let sink = MockSink::arc();
        let broker = Arc::new(BridgeAskUserQuestionBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(1);
        let runner = broker.clone();
        let task = tokio::spawn(async move { runner.run(rx).await });
        let (resp_tx, _resp_rx) = oneshot::channel();
        tx.send(an_exchange(resp_tx)).await.unwrap();

        let original_id = first_request_id(&sink).await;
        assert_eq!(broker.replay_pending().await, 1);
        let replayed: Vec<u64> = sink
            .events()
            .await
            .into_iter()
            .filter_map(|event| match event {
                ClientEvent::AskUserQuestion { request } => Some(request.request_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed,
            vec![original_id, original_id],
            "the replay must reuse the original correlator, not mint a new one"
        );
        // The replayed request is still resolvable exactly once.
        assert!(broker.resolve(original_id, HashMap::new()).await);
        assert!(!broker.resolve(original_id, HashMap::new()).await);
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn drain_drops_parked_reply() {
        let sink = MockSink::arc();
        let broker = Arc::new(BridgeAskUserQuestionBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(1);
        let runner = broker.clone();
        tokio::spawn(async move { runner.run(rx).await });
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AskUserQuestionExchange {
            questions: Vec::new(),
            timeout_secs: None,
            resp_tx,
        })
        .await
        .unwrap();
        while broker.pending_count().await == 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(broker.drain().await, 1);
        assert!(resp_rx.await.is_err());
        assert!(sink.events().await.into_iter().any(|event| matches!(
            event,
            ClientEvent::AskUserQuestionResolved { request_id: 1 }
        )));
    }

    #[tokio::test]
    async fn cancel_closed_drops_only_requests_with_closed_tool_receivers() {
        let sink = MockSink::arc();
        let broker = Arc::new(BridgeAskUserQuestionBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(2);
        let runner = broker.clone();
        tokio::spawn(async move { runner.run(rx).await });

        let (closed_tx, closed_rx) = oneshot::channel();
        tx.send(an_exchange(closed_tx)).await.unwrap();
        let (open_tx, mut open_rx) = oneshot::channel();
        tx.send(an_exchange(open_tx)).await.unwrap();
        while broker.pending_count().await < 2 {
            tokio::task::yield_now().await;
        }

        drop(closed_rx);
        assert_eq!(broker.cancel_closed().await, 1);
        assert_eq!(broker.pending_count().await, 1);
        assert!(matches!(
            open_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(sink.events().await.into_iter().any(|event| matches!(
            event,
            ClientEvent::AskUserQuestionResolved { request_id: 1 }
        )));
        drop(tx);
    }

    #[tokio::test]
    async fn timeout_resolves_with_no_fabricated_client_answer() {
        let sink = MockSink::arc();
        let broker = Arc::new(BridgeAskUserQuestionBroker::new(sink.clone()));
        let (tx, rx) = mpsc::channel(1);
        let runner = broker.clone();
        tokio::spawn(async move { runner.run(rx).await });
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AskUserQuestionExchange {
            questions: vec![AskQuestion {
                question: "Choose?".into(),
                header: "Choice".into(),
                options: vec![AskOption::new("A", "first"), AskOption::new("B", "second")],
                multi_select: false,
            }],
            timeout_secs: Some(0),
            resp_tx,
        })
        .await
        .unwrap();

        let answers = resp_rx.await.unwrap();
        assert!(answers.is_empty());
        assert_eq!(broker.pending_count().await, 0);
        let events = sink.events().await;
        assert!(matches!(
            events.as_slice(),
            [
                ClientEvent::AskUserQuestion { .. },
                ClientEvent::AskUserQuestionResolved { .. }
            ]
        ));
    }

    #[tokio::test]
    async fn resolve_of_unknown_id_is_a_noop() {
        let sink = MockSink::arc();
        let broker = BridgeAskUserQuestionBroker::new(sink.clone());
        assert!(!broker.resolve(999, HashMap::new()).await);
        assert!(!broker.cancel(999).await);
        assert_eq!(broker.replay_pending().await, 0);
    }
}
