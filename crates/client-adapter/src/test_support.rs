//! Reusable test doubles for the adapter.
//!
//! [`MockSink`] is a [`ClientEventSink`] that records every emitted
//! [`ClientEvent`] into an in-memory log. The live-turn / permission tests
//! (F1-12..F1-14) and the bridge-server / mobile skeletons (F2 / F3) all assert
//! against the captured events, so the mock lives in the library (not behind
//! `#[cfg(test)]`) and is re-exported for integration tests too.

use std::sync::Arc;

use async_trait::async_trait;
use client_protocol::events::ClientEvent;
use tokio::sync::Mutex;

use crate::sink::ClientEventSink;

/// A [`ClientEventSink`] that captures every emitted event for later assertion.
///
/// The capture buffer is a `tokio::sync::Mutex<Vec<ClientEvent>>` so it can be
/// shared across the engine task and the assertion site without blocking the
/// async runtime.
#[derive(Debug, Default)]
pub struct MockSink {
    events: Mutex<Vec<ClientEvent>>,
}

impl MockSink {
    /// Construct an empty mock sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Construct an empty mock sink wrapped in an [`Arc`], ready to hand to an
    /// adapter component that takes `Arc<dyn ClientEventSink>`.
    #[must_use]
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Return a snapshot clone of every event captured so far, in emission
    /// order.
    pub async fn events(&self) -> Vec<ClientEvent> {
        self.events.lock().await.clone()
    }

    /// Return the number of events captured so far.
    pub async fn len(&self) -> usize {
        self.events.lock().await.len()
    }

    /// Return `true` if no event has been captured yet.
    pub async fn is_empty(&self) -> bool {
        self.events.lock().await.is_empty()
    }
}

#[async_trait]
impl ClientEventSink for MockSink {
    async fn emit(&self, event: ClientEvent) {
        self.events.lock().await.push(event);
    }
}
