//! Analytics sink trait and value types consumed by the [`crate::bus::AnalyticsBus`].
//!
//! See spec §26.1 (Telemetry Sink) — sinks are pluggable implementations
//! (`BigQuery`, `Statsig`, OTLP) chosen at platform-init time.

use async_trait::async_trait;
use std::collections::HashMap;

/// Metadata payload attached to a single analytics event.
///
/// Keys prefixed with `_PROTO_` route to privileged proto-tagged columns in
/// BigQuery-backed sinks and MUST be stripped before forwarding to general-
/// access destinations (see [`crate::pii::strip_proto_fields`]).
pub type LogEventMetadata = HashMap<String, AnalyticsValue>;

/// Polymorphic value type for analytics metadata.
///
/// Encoded with `#[serde(untagged)]` so JSON sinks see plain `true`, `42`,
/// `3.14`, `"foo"`, or `null` without a tag wrapper.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum AnalyticsValue {
    /// Boolean value.
    Bool(bool),
    /// Signed 64-bit integer value.
    Int(i64),
    /// 64-bit floating point value.
    Float(f64),
    /// UTF-8 string value.
    String(String),
    /// Explicit null — distinct from "key absent" in JSON output.
    None,
}

/// Sink that receives analytics events emitted by the bus.
///
/// Implementations buffer to their own backend (BigQuery streaming insert,
/// Statsig log_event, OTLP exporter, etc.) and decide whether `log_event` /
/// `log_event_async` block the caller. The bus invokes both in async context.
#[async_trait]
pub trait AnalyticsSink: Send + Sync {
    /// Log an event synchronously (caller awaits sink-internal queueing).
    async fn log_event(&self, name: &str, metadata: LogEventMetadata);

    /// Log an event in fire-and-forget mode — sink should not block the caller
    /// on durable delivery. Behaviour parity with `log_event` is sink-defined.
    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata);

    /// Stable identifier for this sink (used in tracing spans and tests).
    fn name(&self) -> &str;
}
