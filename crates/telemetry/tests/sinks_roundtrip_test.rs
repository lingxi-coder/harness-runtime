//! End-to-end: build an `AnalyticsBus` with each sink in turn, emit a few
//! representative events from each tengu category, assert the sink saw them.

use protocol::Secret;
use std::collections::HashMap;
use std::sync::Arc;
use telemetry::{
    AnalyticsBus, AnalyticsValue, InMemorySink, LogEventMetadata, MockStatsigSink, NoOpSink,
    StatsigSink,
};

#[tokio::test]
async fn noop_sink_e2e_no_crash() {
    let bus = AnalyticsBus::new();
    bus.attach_sink(Arc::new(NoOpSink)).await;
    bus.log_event("tengu_api_request_started", HashMap::new())
        .await;
    bus.log_event("tengu_memory_case_mismatch", HashMap::new())
        .await;
    // No assertions: NoOp has no observable state — the test is a smoke gate.
}

#[tokio::test]
async fn in_memory_sink_e2e_captures_all() {
    let bus = AnalyticsBus::new();
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;

    let mut md = HashMap::new();
    md.insert("model".into(), AnalyticsValue::String("m".into()));
    bus.log_event("tengu_api_request_started", md.clone()).await;
    bus.log_event("tengu_api_success", md.clone()).await;
    bus.log_event("tengu_settings_loaded", md).await;

    let events = sink.events().await;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].name, "tengu_api_request_started");
    assert_eq!(events[2].name, "tengu_settings_loaded");
}

#[tokio::test]
async fn mock_statsig_sink_e2e_strips_proto() {
    let bus = AnalyticsBus::new();
    let sink = Arc::new(MockStatsigSink::new(Secret::new("sdk".into())));
    bus.attach_sink(sink.clone()).await;

    let mut md: LogEventMetadata = HashMap::new();
    md.insert("model".into(), AnalyticsValue::String("m".into()));
    md.insert("_PROTO_path".into(), AnalyticsValue::String("/x".into()));
    bus.log_event("tengu_memory_case_mismatch", md).await;

    let captured = sink.captured_payloads().await;
    assert_eq!(captured.len(), 1);
    let metadata = &captured[0]["metadata"];
    assert!(metadata.get("model").is_some());
    assert!(
        metadata.get("_PROTO_path").is_none(),
        "MockStatsigSink must strip _PROTO_* keys"
    );
}

#[tokio::test]
async fn statsig_flush_shutdown_idempotent() {
    let sink = MockStatsigSink::new(Secret::new("sdk".into()));
    assert!(sink.flush().await.is_ok());
    assert!(sink.shutdown().await.is_ok());
    // Idempotent: re-calling shutdown is still Ok.
    assert!(sink.shutdown().await.is_ok());
}
