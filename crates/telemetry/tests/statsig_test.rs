//! `StatsigSink` trait + `MockStatsigSink` skeleton.

use lingxi_core::types::Secret;
use std::collections::HashMap;
use telemetry::{AnalyticsSink, AnalyticsValue, LogEventMetadata, MockStatsigSink, StatsigSink};

#[tokio::test]
async fn statsig_wire_payload_matches_logstatsigevent_shape() {
    let sink = MockStatsigSink::new(Secret::new("sdk-secret".to_string()));
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "model".into(),
        AnalyticsValue::String("claude-sonnet-4-5".into()),
    );
    md.insert("cost_usd".into(), AnalyticsValue::Int(1_500_000_000));

    let wire = sink.statsig_wire_payload("tengu_api_success", &md, Some(1.5));
    assert_eq!(wire["event_name"], "tengu_api_success");
    assert_eq!(wire["value"], 1.5);
    assert_eq!(wire["metadata"]["model"], "claude-sonnet-4-5");
    assert_eq!(wire["metadata"]["cost_usd"], 1_500_000_000_i64);
}

#[tokio::test]
async fn statsig_wire_payload_null_value_for_no_measurement() {
    let sink = MockStatsigSink::new(Secret::new("sdk-secret".into()));
    let md: LogEventMetadata = HashMap::new();
    let wire = sink.statsig_wire_payload("tengu_api_request_started", &md, None);
    assert!(wire["value"].is_null());
}

#[tokio::test]
async fn statsig_wire_payload_strips_proto_fields_for_general_access() {
    let sink = MockStatsigSink::new(Secret::new("sdk-secret".into()));
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("normal".into(), AnalyticsValue::Int(42));
    md.insert(
        "_PROTO_path".into(),
        AnalyticsValue::String("/etc/secret".into()),
    );
    let wire = sink.statsig_wire_payload("tengu_memory_case_mismatch", &md, None);
    assert!(wire["metadata"].get("normal").is_some());
    assert!(
        wire["metadata"].get("_PROTO_path").is_none(),
        "MockStatsigSink must strip _PROTO_ keys per v3 §26.2",
    );
}

#[tokio::test]
async fn statsig_flush_and_shutdown_return_ok_on_mock() {
    let sink = MockStatsigSink::new(Secret::new("sdk-secret".into()));
    assert!(sink.flush().await.is_ok());
    assert!(sink.shutdown().await.is_ok());
}

#[tokio::test]
async fn statsig_sdk_key_returns_secret_wrapper() {
    let sink = MockStatsigSink::new(Secret::new("sdk-abc".into()));
    // `Secret::expose_secret` is an inherent method on the lingxi-protocol
    // newtype (which wraps `secrecy::SecretBox`), so we don't need to import
    // the `secrecy::ExposeSecret` trait here.
    assert_eq!(sink.sdk_key().expose_secret(), "sdk-abc");
    // Debug must redact.
    let dbg = format!("{:?}", sink.sdk_key());
    assert!(dbg.contains("redacted"), "Secret<String> Debug must redact");
}

#[tokio::test]
async fn mock_statsig_log_event_captures_wire_payload() {
    let sink = MockStatsigSink::new(Secret::new("sdk-secret".into()));
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("k".into(), AnalyticsValue::Bool(true));
    sink.log_event("tengu_api_request_started", md).await;
    let captured = sink.captured_payloads().await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["event_name"], "tengu_api_request_started");
    assert_eq!(captured[0]["metadata"]["k"], true);
    assert_eq!(sink.name(), "mock_statsig");
}
