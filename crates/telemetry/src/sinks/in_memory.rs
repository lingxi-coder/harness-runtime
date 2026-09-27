//! `InMemorySink` — test capture sink. Stores events in an insertion-ordered
//! `Vec<RecordedEvent>` for assertion in integration tests.
//!
//! Used by M3-01..M3-05 integration tests (the "end-to-end emit → sink trip"
//! tests in §6 cannot use `NoOpSink` because it has no observable state).

use crate::sink::{AnalyticsSink, LogEventMetadata};
use async_trait::async_trait;
use std::time::Instant;
use tokio::sync::Mutex;

/// One captured event in [`InMemorySink`]'s history.
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    /// Event name (the `tengu_*` string).
    pub name: String,
    /// Metadata payload as passed to `log_event`.
    pub metadata: LogEventMetadata,
    /// Wall-clock instant the sink received the event.
    pub recorded_at: Instant,
}

/// Test sink that captures every event in order.
#[derive(Debug, Default)]
pub struct InMemorySink {
    events: Mutex<Vec<RecordedEvent>>,
}

impl InMemorySink {
    /// Construct an empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of every event the sink has captured so far.
    ///
    /// The returned `Vec` is a clone — the sink's internal state is untouched.
    pub async fn events(&self) -> Vec<RecordedEvent> {
        self.events.lock().await.clone()
    }

    /// Drop every captured event. Useful between `#[tokio::test]` cases.
    pub async fn clear(&self) {
        self.events.lock().await.clear();
    }
}

#[async_trait]
impl AnalyticsSink for InMemorySink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        self.events.lock().await.push(RecordedEvent {
            name: name.to_string(),
            metadata,
            recorded_at: Instant::now(),
        });
    }

    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "in_memory"
    }
}
