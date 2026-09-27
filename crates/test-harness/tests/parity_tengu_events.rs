//! Parity fixture: lock the full count-locked `tengu_*` event-name registry.
//!
//! v3 §32.6 fixture protocol. The fixture is the single source of truth for
//! the M3-06 event-name list. Any drift from `ALL_EVENT_NAMES` indicates an
//! append-only-violation per spec §7 line 787-789.

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct SamplePayload {
    event_name: String,
    statsig_wire: serde_json::Value,
}

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "_current_audit")]
    current_audit: String,
    #[serde(rename = "_registry_count_lock")]
    registry_count_lock: usize,
    event_names: Vec<String>,
    cost_sample_payloads: Vec<SamplePayload>,
}

#[test]
fn event_names_match_registry_byte_for_byte() {
    let fx: Fixture = load_fixture("tengu_events");
    let registry = telemetry::tengu::ALL_EVENT_NAMES;
    assert_eq!(fx.registry_count_lock, 410);
    assert_eq!(registry.len(), fx.registry_count_lock);
    assert_eq!(
        fx.event_names.len(),
        registry.len(),
        "fixture has {} names; registry has {}",
        fx.event_names.len(),
        registry.len(),
    );
    for (i, (f, r)) in fx.event_names.iter().zip(registry.iter()).enumerate() {
        assert_eq!(
            f, *r,
            "position {i} differs: fixture {f:?} vs registry {r:?}",
        );
    }
}

#[test]
fn current_audit_keeps_unimplemented_mcp_telemetry_visible() {
    let fx: Fixture = load_fixture("tengu_events");
    assert!(
        fx.current_audit.contains("44 of the oracle's 55")
            && fx.current_audit.contains("11 absent"),
        "current audit must retain the confirmed MCP telemetry remainder: {}",
        fx.current_audit
    );
    assert!(
        fx.current_audit.contains("44 of the oracle's 55")
            && fx.current_audit.contains("count-locked registry now pins"),
        "the audit must retain the confirmed registered MCP event names"
    );
    for missing in [
        "instructions_pool_change",
        "dropped_tools_pool_change",
        "skills_funnel",
        "arg_trailing_invoke_suffix",
        "description_contains_toolcall_xml",
        "dialog_choice",
        "multidialog_choice",
        "proxy_needs_approval_retry",
        "registry_fetch",
        "sdk_generation",
        "tripwire",
    ] {
        assert!(
            fx.current_audit.contains(missing),
            "current audit must name missing MCP event {missing}: {}",
            fx.current_audit
        );
    }
}

#[test]
fn cost_sample_payloads_match_mock_statsig_wire() {
    use protocol::Secret;
    use std::collections::HashMap;
    use telemetry::{AnalyticsValue, LogEventMetadata, MockStatsigSink};

    let fx: Fixture = load_fixture("tengu_events");
    let sink = MockStatsigSink::new(Secret::new("test-sdk-key".into()));

    for sample in &fx.cost_sample_payloads {
        // Reconstruct the LogEventMetadata from the sample's metadata object.
        let metadata_obj = sample
            .statsig_wire
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .expect("sample.statsig_wire.metadata is an object");
        let mut md: LogEventMetadata = HashMap::new();
        for (k, v) in metadata_obj {
            let av = match v {
                serde_json::Value::Bool(b) => AnalyticsValue::Bool(*b),
                serde_json::Value::Number(n) if n.is_i64() => {
                    AnalyticsValue::Int(n.as_i64().unwrap())
                }
                serde_json::Value::Number(n) if n.is_f64() => {
                    AnalyticsValue::Float(n.as_f64().unwrap())
                }
                serde_json::Value::String(s) => AnalyticsValue::String(s.clone()),
                serde_json::Value::Null => AnalyticsValue::None,
                other => panic!("unsupported metadata value: {other:?}"),
            };
            md.insert(k.clone(), av);
        }
        let value = sample
            .statsig_wire
            .get("value")
            .and_then(serde_json::Value::as_f64);
        let wire = sink.statsig_wire_payload(&sample.event_name, &md, value);
        assert_eq!(
            wire, sample.statsig_wire,
            "statsig wire JSON for {} drifted from fixture",
            sample.event_name,
        );
    }
}
