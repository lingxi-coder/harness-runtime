//! `NoOpSink` — default sink: logs at `tracing::debug!` level only.
//!
//! No network I/O. Metadata is NOT logged (only event name + key count) so
//! `NoOpSink` cannot leak PII even when callers forget to strip proto fields.

use crate::sink::{AnalyticsSink, LogEventMetadata};
use async_trait::async_trait;

/// Default sink: emit a `tracing::debug!` line and discard the event.
///
/// Constructed via `NoOpSink` (unit struct). Use [`crate::AnalyticsBus::with_default_sink`]
/// to wire a bus with this attached at construction.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpSink;

#[async_trait]
impl AnalyticsSink for NoOpSink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        if tracing::enabled!(tracing::Level::DEBUG) {
            tracing::debug!(
                sink = "noop",
                event = name,
                field_count = metadata.len(),
                "telemetry event discarded by NoOpSink",
            );
        }
    }

    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        // Fire-and-forget = same path as log_event for NoOp; nothing to defer.
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "noop"
    }
}
