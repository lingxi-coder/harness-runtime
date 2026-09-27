//! Verifies `tengu_api_success` fires with the field subset claude-code
//! 2.1.195 emits on its per-request success path (`model`, `inputTokens`,
//! `outputTokens`, `cachedInputTokens`, `uncachedInputTokens`, `durationMs`,
//! `durationMsIncludingRetries`, `attempt`, `costUSD`, `provider`, plus the
//! `??void 0`-conditional `stop_reason` / `requestId`).
//!
//! Strict-parity note: the former `tengu_cost_recorded` event was PORT-ONLY
//! (0 hits in claude-code 2.1.195) and was dropped.

use async_trait::async_trait;
use cost::{emit_api_success, ApiSuccessFields};
use std::sync::{Arc, Mutex};
use telemetry::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};

#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<(String, LogEventMetadata)>>,
}

#[async_trait]
impl AnalyticsSink for CaptureSink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        self.events
            .lock()
            .expect("capture sink lock poisoned")
            .push((name.into(), metadata));
    }
    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        self.events
            .lock()
            .expect("capture sink lock poisoned")
            .push((name.into(), metadata));
    }
    fn name(&self) -> &str {
        "capture"
    }
}

#[tokio::test]
async fn emit_api_success_fires_with_field_subset() {
    let bus = Arc::new(AnalyticsBus::new());
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
        .await;

    emit_api_success(
        &bus,
        &ApiSuccessFields {
            model: "claude-opus-4-6".into(),
            input_tokens: 1_000,
            output_tokens: 500,
            cached_input_tokens: 128,
            uncached_input_tokens: 64,
            duration_ms: 200,
            duration_ms_including_retries: 250,
            attempt: 1,
            cost_nano_usd: 17_500_000, // = $0.0175
            provider: "anthropic".into(),
            stop_reason: Some("end_turn".into()),
            request_id: Some("req_abc".into()),
            message_count: 3,
            message_tokens: 4_096,
            did_fall_back_to_non_streaming: false,
            is_non_interactive_session: false,
            print: false,
            is_tty: false,
            query_source: "repl_main_thread".into(),
            permission_mode: "default".into(),
            ttft_ms: Some(42),
            fast_mode: true,
            time_since_last_api_call_ms: Some(1_234),
        },
    )
    .await;

    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one event must fire");
    let (name, payload) = &events[0];
    assert_eq!(
        name, "tengu_api_success",
        "event name must match claude-code 2.1.195 byte-for-byte"
    );

    // The full inserted key set (camelCase claude names) when both optional
    // fields are present.
    let expected_keys = [
        "model",
        "inputTokens",
        "outputTokens",
        "cachedInputTokens",
        "uncachedInputTokens",
        "durationMs",
        "durationMsIncludingRetries",
        "attempt",
        "costUSD",
        "provider",
        "stop_reason",
        "requestId",
        "messageCount",
        "messageTokens",
        "didFallBackToNonStreaming",
        "isNonInteractiveSession",
        "print",
        "isTTY",
        "querySource",
        "permissionMode",
        "ttftMs",
        // `buildAgeMins:THl()` — present because `cost/build.rs` stamps
        // `LINGXI_COST_BUILD_EPOCH_SECS` for THIS crate's compile.
        "buildAgeMins",
        "fastMode",
        "timeSinceLastApiCallMs",
    ];
    for k in &expected_keys {
        assert!(
            payload.contains_key(*k),
            "payload must contain key {k}; got {:?}",
            payload.keys().collect::<Vec<_>>(),
        );
    }
    assert_eq!(
        payload.len(),
        expected_keys.len(),
        "payload must contain EXACTLY the inserted keys; extras: {:?}",
        payload
            .keys()
            .filter(|k| !expected_keys.contains(&k.as_str()))
            .collect::<Vec<_>>(),
    );

    // Value-shape assertions.
    match &payload["model"] {
        AnalyticsValue::String(s) => assert_eq!(s, "claude-opus-4-6"),
        other => panic!("model must be String, got {other:?}"),
    }
    match &payload["inputTokens"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 1_000),
        other => panic!("inputTokens must be Int, got {other:?}"),
    }
    match &payload["outputTokens"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 500),
        other => panic!("outputTokens must be Int, got {other:?}"),
    }
    match &payload["cachedInputTokens"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 128),
        other => panic!("cachedInputTokens must be Int, got {other:?}"),
    }
    match &payload["uncachedInputTokens"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 64),
        other => panic!("uncachedInputTokens must be Int, got {other:?}"),
    }
    match &payload["durationMs"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 200),
        other => panic!("durationMs must be Int, got {other:?}"),
    }
    match &payload["durationMsIncludingRetries"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 250),
        other => panic!("durationMsIncludingRetries must be Int, got {other:?}"),
    }
    match &payload["attempt"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 1),
        other => panic!("attempt must be Int, got {other:?}"),
    }
    // claude `costUSD` is DOLLARS as a float (the port stores nano-USD and
    // divides by 1e9): 17_500_000 / 1e9 = 0.0175.
    match &payload["costUSD"] {
        AnalyticsValue::Float(v) => assert!((v - 0.0175).abs() < 1e-12, "costUSD dollars: {v}"),
        other => panic!("costUSD must be Float (dollars), got {other:?}"),
    }
    match &payload["provider"] {
        AnalyticsValue::String(s) => assert_eq!(s, "anthropic"),
        other => panic!("provider must be String, got {other:?}"),
    }
    match &payload["stop_reason"] {
        AnalyticsValue::String(s) => assert_eq!(s, "end_turn"),
        other => panic!("stop_reason must be String, got {other:?}"),
    }
    match &payload["requestId"] {
        AnalyticsValue::String(s) => assert_eq!(s, "req_abc"),
        other => panic!("requestId must be String, got {other:?}"),
    }
    match &payload["messageCount"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 3),
        other => panic!("messageCount must be Int, got {other:?}"),
    }
    match &payload["messageTokens"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 4_096),
        other => panic!("messageTokens must be Int, got {other:?}"),
    }
    match &payload["didFallBackToNonStreaming"] {
        AnalyticsValue::Bool(b) => assert!(!*b),
        other => panic!("didFallBackToNonStreaming must be Bool, got {other:?}"),
    }
    match &payload["isNonInteractiveSession"] {
        AnalyticsValue::Bool(b) => assert!(!*b),
        other => panic!("isNonInteractiveSession must be Bool, got {other:?}"),
    }
    match &payload["print"] {
        AnalyticsValue::Bool(b) => assert!(!*b),
        other => panic!("print must be Bool, got {other:?}"),
    }
    match &payload["isTTY"] {
        AnalyticsValue::Bool(b) => assert!(!*b),
        other => panic!("isTTY must be Bool, got {other:?}"),
    }
    match &payload["querySource"] {
        AnalyticsValue::String(s) => assert_eq!(s, "repl_main_thread"),
        other => panic!("querySource must be String, got {other:?}"),
    }
    match &payload["permissionMode"] {
        AnalyticsValue::String(s) => assert_eq!(s, "default"),
        other => panic!("permissionMode must be String, got {other:?}"),
    }
    match &payload["ttftMs"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 42),
        other => panic!("ttftMs must be Int, got {other:?}"),
    }
    match &payload["fastMode"] {
        AnalyticsValue::Bool(b) => assert!(*b),
        other => panic!("fastMode must be Bool, got {other:?}"),
    }
    match &payload["timeSinceLastApiCallMs"] {
        AnalyticsValue::Int(n) => assert_eq!(*n, 1_234),
        other => panic!("timeSinceLastApiCallMs must be Int, got {other:?}"),
    }
    // buildAgeMins value is wall-clock-derived (non-deterministic); assert TYPE
    // only, never an exact minutes value.
    match &payload["buildAgeMins"] {
        AnalyticsValue::Int(_) => {}
        other => panic!("buildAgeMins must be Int, got {other:?}"),
    }
}

#[tokio::test]
async fn emit_api_success_omits_absent_optional_fields() {
    // claude `stop_reason:...??void 0` and `requestId:...??void 0` → the key
    // is OMITTED (not None) when the value is absent.
    let bus = Arc::new(AnalyticsBus::new());
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
        .await;

    emit_api_success(
        &bus,
        &ApiSuccessFields {
            model: "claude-unknown".into(),
            input_tokens: 100,
            output_tokens: 50,
            cached_input_tokens: 0,
            uncached_input_tokens: 0,
            duration_ms: 10,
            duration_ms_including_retries: 10,
            attempt: 1,
            cost_nano_usd: 0,
            provider: "anthropic".into(),
            stop_reason: None,
            request_id: None,
            message_count: 1,
            message_tokens: 0,
            did_fall_back_to_non_streaming: false,
            is_non_interactive_session: false,
            print: false,
            is_tty: false,
            query_source: "repl_main_thread".into(),
            permission_mode: "default".into(),
            ttft_ms: None,
            fast_mode: false,
            time_since_last_api_call_ms: None,
        },
    )
    .await;

    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1, "event must fire even at zero cost");
    let payload = &events[0].1;
    assert!(
        !payload.contains_key("stop_reason"),
        "absent stop_reason must be OMITTED (claude ??void 0)"
    );
    assert!(
        !payload.contains_key("requestId"),
        "absent requestId must be OMITTED (claude ??void 0)"
    );
    assert!(
        !payload.contains_key("ttftMs"),
        "absent ttft_ms must OMIT ttftMs (claude ttftMs:l??void 0)"
    );
    assert!(
        !payload.contains_key("timeSinceLastApiCallMs"),
        "first-call None must OMIT timeSinceLastApiCallMs (claude W=...:void 0)"
    );
    // fastMode is a BARE unconditional bool — always present, even at zero cost.
    match &payload["fastMode"] {
        AnalyticsValue::Bool(b) => assert!(!*b),
        other => panic!("fastMode must be Bool, got {other:?}"),
    }
    // costUSD is dollars float; 0 nano-USD → 0.0.
    match &payload["costUSD"] {
        AnalyticsValue::Float(v) => assert!(v.abs() < 1e-12, "costUSD must be 0.0, got {v}"),
        other => panic!("costUSD must be Float(0.0), got {other:?}"),
    }
}
