//! `StatsigSink` trait + `MockStatsigSink` reference impl.
//!
//! Spec §8 M3-06 phase 11. Real provider impls live in platform crates;
//! this module ships only the trait surface and a mock with the wire
//! format locked.
//!
//! Wire shape (claude-code @ 6a25909 `src/services/statsig.ts::logStatsigEvent`):
//! `{"event_name": "<name>", "value": <number|null>, "metadata": <object>}`.

use crate::error::TelemetryError;
use crate::pii::strip_proto_fields;
use crate::sink::{AnalyticsSink, AnalyticsValue, LogEventMetadata};
use async_trait::async_trait;
use lingxi_core::types::Secret;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

/// Statsig-shaped sink contract. Spec line 921-929.
///
/// Adapters implement [`AnalyticsSink`] + this trait. Engine init code that
/// wants Statsig routing downcasts via `Arc<dyn AnalyticsSink>::downcast`-able
/// wrappers (or constructs the concrete sink at platform-init time).
#[async_trait]
pub trait StatsigSink: AnalyticsSink {
    /// Return the SDK key, wrapped in [`Secret<String>`] so `Debug`/`Display`
    /// always redact.
    fn sdk_key(&self) -> &Secret<String>;

    /// Force-flush any buffered events. Real impls block until the upstream
    /// confirms delivery; the mock returns `Ok(())` immediately.
    async fn flush(&self) -> Result<(), TelemetryError>;

    /// Tear the sink down; same semantics as `flush` plus releases any
    /// background tasks. Idempotent.
    async fn shutdown(&self) -> Result<(), TelemetryError>;
}

/// Reference mock impl. Captures events to an in-memory `Vec`, builds the
/// canonical wire JSON via [`Self::statsig_wire_payload`], and provides the
/// `flush` / `shutdown` no-op stubs.
pub struct MockStatsigSink {
    sdk_key: Secret<String>,
    captured: Mutex<Vec<Value>>,
}

impl MockStatsigSink {
    /// Construct with a (zeroed-on-drop) SDK key.
    #[must_use]
    pub fn new(sdk_key: Secret<String>) -> Self {
        Self {
            sdk_key,
            captured: Mutex::new(Vec::new()),
        }
    }

    /// Build the Statsig `logStatsigEvent` wire-JSON payload.
    ///
    /// `value` is `Some(numeric primary measurement)` for events with a single
    /// dominant number (e.g. cost USD, duration); `None` for events that have
    /// no scalar primary (most events). `metadata` is the raw `LogEventMetadata`
    /// from the bus; `_PROTO_*` keys are stripped before serialization.
    #[must_use]
    pub fn statsig_wire_payload(
        &self,
        event_name: &str,
        metadata: &LogEventMetadata,
        value: Option<f64>,
    ) -> Value {
        let mut md = metadata.clone();
        strip_proto_fields(&mut md);
        let metadata_value: Map<String, Value> = md
            .into_iter()
            .map(|(k, v)| (k, analytics_value_to_json(&v)))
            .collect();
        json!({
            "event_name": event_name,
            "value": value,
            "metadata": Value::Object(metadata_value),
        })
    }

    /// Return a snapshot of every captured wire payload (for test assertion).
    pub async fn captured_payloads(&self) -> Vec<Value> {
        self.captured.lock().await.clone()
    }
}

fn analytics_value_to_json(v: &AnalyticsValue) -> Value {
    match v {
        AnalyticsValue::Bool(b) => Value::Bool(*b),
        AnalyticsValue::Int(i) => Value::Number((*i).into()),
        AnalyticsValue::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        AnalyticsValue::String(s) => Value::String(s.clone()),
        AnalyticsValue::None => Value::Null,
    }
}

#[async_trait]
impl AnalyticsSink for MockStatsigSink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        let wire = self.statsig_wire_payload(name, &metadata, None);
        self.captured.lock().await.push(wire);
    }

    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "mock_statsig"
    }
}

#[async_trait]
impl StatsigSink for MockStatsigSink {
    fn sdk_key(&self) -> &Secret<String> {
        &self.sdk_key
    }

    async fn flush(&self) -> Result<(), TelemetryError> {
        // Mock: nothing to flush; real impl awaits upstream confirmation.
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), TelemetryError> {
        // Mock: clear buffer and return.
        self.captured.lock().await.clear();
        Ok(())
    }
}
