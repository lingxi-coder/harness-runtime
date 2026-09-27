//! Parity fixture: `tengu_api_success` event name + payload field subset locked
//! against Claude Code 2.1.246's per-request success emission, plus the
//! surviving `tengu_cost_budget_*` thresholds.
//!
//! Strict-parity note: the former `tengu_cost_recorded` event was PORT-ONLY
//! (0 hits in claude-code 2.1.195) and was dropped; the per-request success
//! telemetry is now `tengu_api_success` (`cost::emit_api_success`). The
//! HashMap-backed sink does not byte-lock field order — only the field-name
//! SET + value types/values are observable.

use async_trait::async_trait;
use cost::{
    emit_api_success, ApiSuccessFields, BATCH_DISCOUNT_BPS, BUDGET_EXCEEDED_THRESHOLD_BPS,
    BUDGET_WARNING_THRESHOLD_BPS,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use telemetry::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    event_names: EventNames,
    thresholds_bps: Thresholds,
    query_sources: QuerySources,
    canonical_api_success_input: CanonicalInput,
    expected_payload_keys: Vec<String>,
    expected_payload_values: HashMap<String, serde_json::Value>,
    budget_warning_payload_keys: Vec<String>,
    budget_exceeded_payload_keys: Vec<String>,
}

#[derive(Deserialize)]
struct QuerySources {
    repl_main_thread: String,
    sdk: String,
}

#[derive(Deserialize)]
struct EventNames {
    api_success: String,
    budget_warning: String,
    budget_exceeded: String,
}

#[derive(Deserialize)]
struct Thresholds {
    warning: u32,
    exceeded: u32,
    batch_discount: u32,
}

#[derive(Deserialize)]
struct CanonicalInput {
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    duration_ms: u64,
    duration_ms_including_retries: u64,
    attempt: u32,
    cost_nano_usd: u64,
    provider: String,
    stop_reason: String,
    request_id: String,
    message_count: u32,
    message_tokens: u64,
    did_fall_back_to_non_streaming: bool,
    is_non_interactive_session: bool,
    print: bool,
    is_tty: bool,
    query_source: String,
    permission_mode: String,
    ttft_ms: u64,
    fast_mode: bool,
    time_since_last_api_call_ms: u64,
}

#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<(String, LogEventMetadata)>>,
}

#[async_trait]
impl AnalyticsSink for CaptureSink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        self.events.lock().unwrap().push((name.into(), metadata));
    }
    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        self.events.lock().unwrap().push((name.into(), metadata));
    }
    fn name(&self) -> &str {
        "capture"
    }
}

#[test]
fn event_names_match_fixture_byte_for_byte() {
    let fx: Fixture = load_fixture("cost_events");
    assert_eq!(
        fx.event_names.api_success, "tengu_api_success",
        "fixture must declare the Claude Code 2.1.246 api_success event name"
    );
    assert_eq!(fx.event_names.budget_warning, "tengu_cost_budget_warning");
    assert_eq!(fx.event_names.budget_exceeded, "tengu_cost_budget_exceeded");
}

#[test]
fn thresholds_match_constants_byte_for_byte() {
    let fx: Fixture = load_fixture("cost_events");
    assert_eq!(
        fx.thresholds_bps.warning, BUDGET_WARNING_THRESHOLD_BPS,
        "fixture warning threshold must equal BUDGET_WARNING_THRESHOLD_BPS"
    );
    assert_eq!(
        fx.thresholds_bps.exceeded, BUDGET_EXCEEDED_THRESHOLD_BPS,
        "fixture exceeded threshold must equal BUDGET_EXCEEDED_THRESHOLD_BPS"
    );
    assert_eq!(
        fx.thresholds_bps.batch_discount, BATCH_DISCOUNT_BPS,
        "fixture batch_discount must equal BATCH_DISCOUNT_BPS"
    );
}

#[test]
fn fixture_query_source_matches_the_live_repl_orchestrator_config() {
    let fx: Fixture = load_fixture("cost_events");
    let config = orchestrator::OrchestratorConfig::default();
    assert_eq!(
        fx.canonical_api_success_input.query_source,
        config.query_source
    );
    assert_eq!(
        config.query_source,
        orchestrator::QUERY_SOURCE_REPL_MAIN_THREAD
    );
    assert_eq!(fx.query_sources.repl_main_thread, config.query_source);
    assert_eq!(fx.query_sources.sdk, orchestrator::QUERY_SOURCE_SDK);
    assert_eq!(
        orchestrator::sanitize_query_source(&fx.query_sources.sdk),
        orchestrator::QUERY_SOURCE_SDK
    );
}

#[tokio::test]
async fn api_success_sdk_query_source_matches_transport_wire_value() {
    let fx: Fixture = load_fixture("cost_events");
    let bus = Arc::new(AnalyticsBus::new());
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
        .await;
    let inp = &fx.canonical_api_success_input;

    emit_api_success(
        &bus,
        &ApiSuccessFields {
            model: inp.model.clone(),
            input_tokens: inp.input_tokens,
            output_tokens: inp.output_tokens,
            cached_input_tokens: inp.cached_input_tokens,
            uncached_input_tokens: inp.uncached_input_tokens,
            duration_ms: inp.duration_ms,
            duration_ms_including_retries: inp.duration_ms_including_retries,
            attempt: inp.attempt,
            cost_nano_usd: inp.cost_nano_usd,
            provider: inp.provider.clone(),
            stop_reason: Some(inp.stop_reason.clone()),
            request_id: Some(inp.request_id.clone()),
            message_count: inp.message_count,
            message_tokens: inp.message_tokens,
            did_fall_back_to_non_streaming: inp.did_fall_back_to_non_streaming,
            is_non_interactive_session: true,
            print: false,
            is_tty: false,
            query_source: fx.query_sources.sdk.clone(),
            permission_mode: inp.permission_mode.clone(),
            ttft_ms: Some(inp.ttft_ms),
            fast_mode: inp.fast_mode,
            time_since_last_api_call_ms: Some(inp.time_since_last_api_call_ms),
        },
    )
    .await;

    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].1.get("querySource"),
        Some(AnalyticsValue::String(value)) if value == "sdk"
    ));
}

#[tokio::test]
async fn api_success_payload_matches_fixture_byte_for_byte() {
    let fx: Fixture = load_fixture("cost_events");

    let bus = Arc::new(AnalyticsBus::new());
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
        .await;

    let inp = &fx.canonical_api_success_input;
    emit_api_success(
        &bus,
        &ApiSuccessFields {
            model: inp.model.clone(),
            input_tokens: inp.input_tokens,
            output_tokens: inp.output_tokens,
            cached_input_tokens: inp.cached_input_tokens,
            uncached_input_tokens: inp.uncached_input_tokens,
            duration_ms: inp.duration_ms,
            duration_ms_including_retries: inp.duration_ms_including_retries,
            attempt: inp.attempt,
            cost_nano_usd: inp.cost_nano_usd,
            provider: inp.provider.clone(),
            stop_reason: Some(inp.stop_reason.clone()),
            request_id: Some(inp.request_id.clone()),
            message_count: inp.message_count,
            message_tokens: inp.message_tokens,
            did_fall_back_to_non_streaming: inp.did_fall_back_to_non_streaming,
            is_non_interactive_session: inp.is_non_interactive_session,
            print: inp.print,
            is_tty: inp.is_tty,
            query_source: inp.query_source.clone(),
            permission_mode: inp.permission_mode.clone(),
            ttft_ms: Some(inp.ttft_ms),
            fast_mode: inp.fast_mode,
            time_since_last_api_call_ms: Some(inp.time_since_last_api_call_ms),
        },
    )
    .await;

    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one api_success event");
    assert_eq!(events[0].0, fx.event_names.api_success);
    let payload = &events[0].1;

    // Every expected key is present and matches its expected value.
    for key in &fx.expected_payload_keys {
        let actual = payload
            .get(key)
            .unwrap_or_else(|| panic!("payload missing key {key}"));
        // `buildAgeMins:THl()` is wall-clock-derived (non-deterministic): assert
        // PRESENCE + Int type only, never an exact minutes value.
        if key == "buildAgeMins" {
            assert!(
                matches!(actual, AnalyticsValue::Int(_)),
                "buildAgeMins must be Int, got {actual:?}"
            );
            continue;
        }
        let expected = fx
            .expected_payload_values
            .get(key)
            .unwrap_or_else(|| panic!("fixture missing expected value for key {key}"));
        match (actual, expected) {
            (AnalyticsValue::String(a), serde_json::Value::String(e)) => {
                assert_eq!(a, e, "key {key} string mismatch");
            }
            (AnalyticsValue::Int(a), serde_json::Value::Number(e)) => {
                let e_i64 = e.as_i64().expect("fixture int");
                assert_eq!(*a, e_i64, "key {key} int mismatch");
            }
            // costUSD is a DOLLARS float (claude `costUSD:p`).
            (AnalyticsValue::Float(a), serde_json::Value::Number(e)) => {
                let e_f64 = e.as_f64().expect("fixture float");
                assert!((a - e_f64).abs() < 1e-12, "key {key} float mismatch");
            }
            (AnalyticsValue::Bool(a), serde_json::Value::Bool(e)) => {
                assert_eq!(*a, *e, "key {key} bool mismatch");
            }
            (a, e) => panic!("key {key}: type mismatch — got {a:?}, fixture {e:?}"),
        }
    }
    // No extra keys.
    assert_eq!(
        payload.len(),
        fx.expected_payload_keys.len(),
        "payload has extra keys: {:?}",
        payload
            .keys()
            .filter(|k| !fx.expected_payload_keys.contains(k))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn fixture_declares_budget_payload_key_sets() {
    let fx: Fixture = load_fixture("cost_events");
    assert_eq!(
        fx.budget_warning_payload_keys,
        vec!["limit_usd", "current_usd", "percent_bps"],
        "warning payload has exactly 3 keys per spec §7 line 735"
    );
    assert_eq!(
        fx.budget_exceeded_payload_keys,
        vec!["limit_usd", "current_usd"],
        "exceeded payload has exactly 2 keys per spec §7 line 736"
    );
}
