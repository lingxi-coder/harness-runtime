//! `MockHttpTransport` — scripted response store for deterministic tests.
//!
//! Tests register responses for URL+method pairs, then assert the engine
//! consumes them in the expected order. Use `assert_drained` to verify no
//! response was left unconsumed at the end of a test.

#![allow(clippy::unwrap_used)]
// Plan keeps the explicit `Some(Sync(_)) | Some(SyncErr(_))` form for clarity.
#![allow(clippy::unnested_or_patterns)]

use async_trait::async_trait;
use futures_core::stream::Stream;
use platform_api::{HttpError, HttpTransport, ResolvedAddressOverride};
use protocol::{HttpRequest, HttpResponse, SseEvent};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// A pre-recorded response that the mock transport hands back on the next call.
#[derive(Debug, Clone)]
pub enum ScriptedResponse {
    /// A complete (non-streaming) HTTP response to deliver from
    /// [`HttpTransport::request`].
    Sync(HttpResponse),
    /// A transport-level error to surface from [`HttpTransport::request`].
    SyncErr(HttpError),
    /// SSE stream — vec of complete events that will be yielded one per poll.
    Stream(Vec<SseEvent>),
}

/// In-memory mock of [`HttpTransport`] that returns scripted responses in FIFO
/// order and records every received request for later assertions.
#[derive(Default)]
pub struct MockHttpTransport {
    queue: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    received: Arc<Mutex<Vec<HttpRequest>>>,
}

impl MockHttpTransport {
    /// Construct an empty mock transport. Call [`Self::enqueue`] to script
    /// responses before exercising the system under test.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `r` to the back of the scripted response queue.
    ///
    /// # Panics
    /// Panics if the internal queue mutex is poisoned (only possible if a
    /// previous test thread panicked while holding the lock).
    pub fn enqueue(&self, r: ScriptedResponse) {
        self.queue.lock().unwrap().push_back(r);
    }

    /// Snapshot every request the mock has received so far, in arrival order.
    ///
    /// # Panics
    /// Panics if the internal received-requests mutex is poisoned.
    #[allow(clippy::must_use_candidate)]
    pub fn received_requests(&self) -> Vec<HttpRequest> {
        self.received.lock().unwrap().clone()
    }

    /// Assert no scripted responses remain. Use at end of test.
    ///
    /// # Panics
    /// Panics if any scripted response remains undelivered, or if the
    /// internal queue mutex is poisoned.
    pub fn assert_drained(&self) {
        let q = self.queue.lock().unwrap();
        assert!(
            q.is_empty(),
            "{} scripted responses left undelivered",
            q.len()
        );
    }
}

#[async_trait]
impl HttpTransport for MockHttpTransport {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.received.lock().unwrap().push(req);
        match self.queue.lock().unwrap().pop_front() {
            Some(ScriptedResponse::Sync(resp)) => Ok(resp),
            Some(ScriptedResponse::SyncErr(err)) => Err(err),
            Some(ScriptedResponse::Stream(_)) => Err(HttpError::InvalidResponse(
                "scripted Stream response on non-stream call".into(),
            )),
            None => Err(HttpError::InvalidResponse(
                "no scripted response available".into(),
            )),
        }
    }

    async fn request_with_resolved_addrs(
        &self,
        req: HttpRequest,
        _resolved: Option<ResolvedAddressOverride>,
    ) -> Result<HttpResponse, HttpError> {
        self.request(req).await
    }

    async fn stream_sse(
        &self,
        req: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
        self.received.lock().unwrap().push(req);
        match self.queue.lock().unwrap().pop_front() {
            Some(ScriptedResponse::Stream(events)) => Ok(Box::pin(ScriptedSseStream {
                remaining: events.into(),
            })),
            Some(ScriptedResponse::Sync(_)) | Some(ScriptedResponse::SyncErr(_)) => Err(
                HttpError::InvalidResponse("scripted non-stream response on stream call".into()),
            ),
            None => Err(HttpError::InvalidResponse(
                "no scripted response available".into(),
            )),
        }
    }
}

struct ScriptedSseStream {
    remaining: VecDeque<SseEvent>,
}

impl Stream for ScriptedSseStream {
    type Item = Result<SseEvent, HttpError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.remaining.pop_front().map(Ok))
    }
}
