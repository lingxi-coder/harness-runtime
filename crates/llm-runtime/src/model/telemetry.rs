//! Helpers that emit the five `tengu_api_*` telemetry events through the
//! optional `AnalyticsBus`. Each helper short-circuits when the bus is `None`
//! so test setups that don't attach a sink pay no cost.
//!
//! Ported verbatim from `api-client/src/anthropic.rs`'s internal `telemetry`
//! module. Event names and key names are spec-locked; see spec §7.
//!
//! Payload keys:
//! * `model`, `request_id`, `error_kind` use the [`Verified`] newtype.
//! * `status_code` becomes `AnalyticsValue::None` when absent, never omitted.
//! * `retry_after_ms`, `duration_ms`, token counts and `attempt` use
//!   `AnalyticsValue::Int` (cast from the concrete unsigned type).

use std::sync::Arc;
use telemetry::{AnalyticsBus, AnalyticsValue, LogEventMetadata, Verified};

/// Emit `tengu_api_request_started` through the optional bus.
///
/// No-op when `bus` is `None` (test-mode default).
#[allow(
    clippy::cast_possible_wrap,
    reason = "duration_ms / retry_after_ms fit in i64 for all realistic deployments"
)]
pub async fn emit_started(
    bus: &Option<Arc<AnalyticsBus>>,
    model: &str,
    request_id: &str,
    stream: bool,
) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    m.insert(
        "model".into(),
        AnalyticsValue::String(
            Verified::assert_safe(model.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "request_id".into(),
        AnalyticsValue::String(
            Verified::assert_safe(request_id.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert("stream".into(), AnalyticsValue::Bool(stream));
    bus.log_event("tengu_api_request_started", m).await;
}

/// Emit the internal purpose of a side query without adding it to the provider
/// wire request. Main-turn requests leave this event absent.
pub async fn emit_query_source(bus: &Option<Arc<AnalyticsBus>>, model: &str, query_source: &str) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    m.insert(
        "model".into(),
        AnalyticsValue::String(Verified::assert_safe(model.to_string()).into_inner()),
    );
    m.insert(
        "querySource".into(),
        AnalyticsValue::String(Verified::assert_safe(query_source.to_string()).into_inner()),
    );
    bus.log_event("tengu_api_query_source", m).await;
}

/// Emit `tengu_api_request_succeeded` with wall-clock duration and HTTP status.
///
/// No-op when `bus` is `None`.
#[allow(
    clippy::cast_possible_wrap,
    reason = "duration_ms fits in i64 for all realistic deployments"
)]
pub async fn emit_succeeded(
    bus: &Option<Arc<AnalyticsBus>>,
    model: &str,
    request_id: &str,
    duration_ms: u64,
    status: u16,
) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    m.insert(
        "model".into(),
        AnalyticsValue::String(
            Verified::assert_safe(model.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "request_id".into(),
        AnalyticsValue::String(
            Verified::assert_safe(request_id.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    m.insert("status".into(), AnalyticsValue::Int(i64::from(status)));
    bus.log_event("tengu_api_request_succeeded", m).await;
}

/// Emit `tengu_api_request_failed` with a stable `error_kind` label and an
/// optional HTTP status code (`AnalyticsValue::None` when absent).
///
/// No-op when `bus` is `None`.
pub async fn emit_failed(
    bus: &Option<Arc<AnalyticsBus>>,
    model: &str,
    request_id: &str,
    error_kind: &str,
    status_code: Option<u16>,
) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    m.insert(
        "model".into(),
        AnalyticsValue::String(
            Verified::assert_safe(model.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "request_id".into(),
        AnalyticsValue::String(
            Verified::assert_safe(request_id.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "error_kind".into(),
        AnalyticsValue::String(
            Verified::assert_safe(error_kind.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "status_code".into(),
        match status_code {
            Some(s) => AnalyticsValue::Int(i64::from(s)),
            None => AnalyticsValue::None,
        },
    );
    bus.log_event("tengu_api_request_failed", m).await;
}

/// Emit `tengu_api_rate_limited` with the resolved retry-after delay in
/// milliseconds.
///
/// No-op when `bus` is `None`.
#[allow(
    clippy::cast_possible_wrap,
    reason = "retry_after_ms fits in i64 for all realistic deployments"
)]
pub async fn emit_rate_limited(bus: &Option<Arc<AnalyticsBus>>, model: &str, retry_after_ms: u64) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    m.insert(
        "model".into(),
        AnalyticsValue::String(
            Verified::assert_safe(model.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "retry_after_ms".into(),
        AnalyticsValue::Int(retry_after_ms as i64),
    );
    bus.log_event("tengu_api_rate_limited", m).await;
}

/// Emit `tengu_max_tokens_context_overflow_adjustment` (claude-code
/// `withRetry.ts:418`) with the parsed input/limit and the recomputed cap.
/// The bus payload is not byte-compared (Batch 5 fidelity note), but the
/// event name and field keys mirror the TS `logEvent` call.
#[allow(
    clippy::cast_possible_wrap,
    reason = "token counts and attempt index fit in i64 for all realistic deployments"
)]
pub async fn emit_max_tokens_overflow_adjustment(
    bus: &Option<Arc<AnalyticsBus>>,
    model: &str,
    input_tokens: u64,
    context_limit: u64,
    adjusted_max_tokens: u32,
    attempt: u32,
) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    m.insert(
        "model".into(),
        AnalyticsValue::String(
            Verified::assert_safe(model.to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "inputTokens".into(),
        AnalyticsValue::Int(input_tokens as i64),
    );
    m.insert(
        "contextLimit".into(),
        AnalyticsValue::Int(context_limit as i64),
    );
    m.insert(
        "adjustedMaxTokens".into(),
        AnalyticsValue::Int(i64::from(adjusted_max_tokens)),
    );
    m.insert("attempt".into(), AnalyticsValue::Int(i64::from(attempt)));
    bus.log_event("tengu_max_tokens_context_overflow_adjustment", m)
        .await;
}

/// Emit `tengu_thinking_signature_strip_retry` (claude-code 2.1.259): the
/// server rejected an Anthropic thinking signature, so the client stripped
/// thinking blocks and retries.
///
/// No-op when `bus` is `None`.
pub async fn emit_thinking_signature_strip_retry(
    bus: &Option<Arc<AnalyticsBus>>,
    query_source: Option<&str>,
    model: &str,
    stripped_signed_count: u32,
    stripped_unsigned_count: u32,
) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    if let Some(query_source) = query_source {
        m.insert(
            "query_source".into(),
            AnalyticsValue::String(Verified::assert_safe(query_source.to_string()).into_inner()),
        );
    }
    m.insert(
        "model".into(),
        AnalyticsValue::String(Verified::assert_safe(model.to_string()).into_inner()),
    );
    m.insert(
        "stripped_signed_count".into(),
        AnalyticsValue::Int(i64::from(stripped_signed_count)),
    );
    m.insert(
        "stripped_unsigned_count".into(),
        AnalyticsValue::Int(i64::from(stripped_unsigned_count)),
    );
    bus.log_event("tengu_thinking_signature_strip_retry", m)
        .await;
}

/// Current native dispatch recovery metadata; absent optional fields are omitted.
pub struct DispatchFallbackEvent<'a> {
    pub model: &'a str,
    pub dispatch: Option<&'a str>,
    pub reason: &'static str,
    pub status: Option<u16>,
    pub query_source: Option<&'a str>,
    pub request_id: Option<&'a str>,
}

pub async fn emit_dispatch_header_fallback(
    bus: &Option<Arc<AnalyticsBus>>,
    event: DispatchFallbackEvent<'_>,
) {
    let Some(bus) = bus else { return };
    let mut m = LogEventMetadata::new();
    for (key, value) in [
        ("model", event.model),
        ("dispatch", event.dispatch.unwrap_or("none")),
        ("resend_dispatch", "v2p"),
        ("reason", event.reason),
    ] {
        m.insert(key.into(), AnalyticsValue::String(value.into()));
    }
    m.insert(
        "status".into(),
        event.status.map_or_else(
            || AnalyticsValue::String("none".into()),
            |status| AnalyticsValue::Int(i64::from(status)),
        ),
    );
    for (key, value) in [
        ("query_source", event.query_source),
        ("request_id", event.request_id),
    ] {
        if let Some(value) = value {
            m.insert(key.into(), AnalyticsValue::String(value.into()));
        }
    }
    bus.log_event("tengu_dispatch_header_fallback", m).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, AnalyticsValue, InMemorySink};

    /// Helper: build a bus with an `InMemorySink` attached and return both.
    async fn bus_with_sink() -> (Arc<AnalyticsBus>, Arc<InMemorySink>) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;
        (bus, sink)
    }

    #[tokio::test]
    async fn emit_started_fires_correct_event_name_and_keys() {
        let (bus, sink) = bus_with_sink().await;
        let opt = Some(bus);
        emit_started(&opt, "claude-sonnet-4-6", "req-abc", false).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.name, "tengu_api_request_started");
        assert!(
            matches!(&ev.metadata["model"], AnalyticsValue::String(s) if s == "claude-sonnet-4-6")
        );
        assert!(matches!(&ev.metadata["request_id"], AnalyticsValue::String(s) if s == "req-abc"));
        assert!(matches!(
            &ev.metadata["stream"],
            AnalyticsValue::Bool(false)
        ));
    }

    #[tokio::test]
    async fn emit_started_with_none_bus_is_silent() {
        // Should not panic and produce no events.
        emit_started(&None, "model", "req-1", true).await;
    }

    #[tokio::test]
    async fn emit_succeeded_fires_correct_event_name_and_keys() {
        let (bus, sink) = bus_with_sink().await;
        let opt = Some(bus);
        emit_succeeded(&opt, "claude-opus-4-6", "req-xyz", 1234, 200).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.name, "tengu_api_request_succeeded");
        assert!(
            matches!(&ev.metadata["model"], AnalyticsValue::String(s) if s == "claude-opus-4-6")
        );
        assert!(matches!(&ev.metadata["request_id"], AnalyticsValue::String(s) if s == "req-xyz"));
        assert!(matches!(
            &ev.metadata["duration_ms"],
            AnalyticsValue::Int(1234)
        ));
        assert!(matches!(&ev.metadata["status"], AnalyticsValue::Int(200)));
    }

    #[tokio::test]
    async fn emit_succeeded_with_none_bus_is_silent() {
        emit_succeeded(&None, "model", "req-1", 500, 200).await;
    }

    #[tokio::test]
    async fn emit_failed_fires_correct_event_name_and_keys_with_status() {
        let (bus, sink) = bus_with_sink().await;
        let opt = Some(bus);
        emit_failed(
            &opt,
            "claude-haiku-4-5",
            "req-123",
            "rate_limited",
            Some(429),
        )
        .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.name, "tengu_api_request_failed");
        assert!(
            matches!(&ev.metadata["model"], AnalyticsValue::String(s) if s == "claude-haiku-4-5")
        );
        assert!(
            matches!(&ev.metadata["error_kind"], AnalyticsValue::String(s) if s == "rate_limited")
        );
        assert!(matches!(
            &ev.metadata["status_code"],
            AnalyticsValue::Int(429)
        ));
    }

    #[tokio::test]
    async fn emit_failed_uses_none_value_when_no_status() {
        let (bus, sink) = bus_with_sink().await;
        let opt = Some(bus);
        emit_failed(&opt, "model", "req-1", "http", None).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].metadata["status_code"],
            AnalyticsValue::None
        ));
    }

    #[tokio::test]
    async fn emit_failed_with_none_bus_is_silent() {
        emit_failed(&None, "model", "req-1", "http", None).await;
    }

    #[tokio::test]
    async fn emit_rate_limited_fires_correct_event_name_and_keys() {
        let (bus, sink) = bus_with_sink().await;
        let opt = Some(bus);
        emit_rate_limited(&opt, "claude-sonnet-4-6", 5000).await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.name, "tengu_api_rate_limited");
        assert!(
            matches!(&ev.metadata["model"], AnalyticsValue::String(s) if s == "claude-sonnet-4-6")
        );
        assert!(matches!(
            &ev.metadata["retry_after_ms"],
            AnalyticsValue::Int(5000)
        ));
    }

    #[tokio::test]
    async fn emit_rate_limited_with_none_bus_is_silent() {
        emit_rate_limited(&None, "model", 1000).await;
    }

    #[tokio::test]
    async fn emit_max_tokens_overflow_fires_correct_event_name_and_keys() {
        let (bus, sink) = bus_with_sink().await;
        let opt = Some(bus);
        emit_max_tokens_overflow_adjustment(&opt, "claude-opus-4-6", 200_000, 190_000, 3000, 1)
            .await;

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.name, "tengu_max_tokens_context_overflow_adjustment");
        assert!(
            matches!(&ev.metadata["model"], AnalyticsValue::String(s) if s == "claude-opus-4-6")
        );
        assert!(matches!(
            &ev.metadata["inputTokens"],
            AnalyticsValue::Int(200_000)
        ));
        assert!(matches!(
            &ev.metadata["contextLimit"],
            AnalyticsValue::Int(190_000)
        ));
        assert!(matches!(
            &ev.metadata["adjustedMaxTokens"],
            AnalyticsValue::Int(3000)
        ));
        assert!(matches!(&ev.metadata["attempt"], AnalyticsValue::Int(1)));
    }

    #[tokio::test]
    async fn emit_max_tokens_overflow_with_none_bus_is_silent() {
        emit_max_tokens_overflow_adjustment(&None, "model", 100, 90, 3000, 0).await;
    }
}
