//! Parity driver for `settings_merge.json` — v3 §32.6 protocol.
//!
//! Loads the fixture, replays it through `Settings::load_with_observer`,
//! and asserts:
//! - The merged `expected_effective_settings` matches byte-for-byte (after
//!   JSON canonicalization via `serde_json::Value`).
//! - The `expected_provenance` trace matches for every named field.
//! - The `tengu_settings_loaded` telemetry event fires with the locked keys.

use async_trait::async_trait;
use lingxi_core::settings::schema::SettingsJson;
use lingxi_core::settings::{LoadInputs, Settings};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use telemetry::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};
use test_harness::parity::load_fixture;

#[derive(Debug, Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    #[serde(rename = "_source")]
    source: String,
    #[allow(dead_code)]
    #[serde(rename = "_note")]
    note: String,
    input_defaults: serde_json::Value,
    input_project_settings_json: serde_json::Value,
    input_user_settings_json: serde_json::Value,
    input_env: BTreeMap<String, String>,
    expected_effective_settings: serde_json::Value,
    expected_provenance: BTreeMap<String, Vec<String>>,
    expected_telemetry_event_names: Vec<String>,
    expected_telemetry_loaded_keys: Vec<String>,
}

#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<(String, LogEventMetadata)>>,
}

#[async_trait]
impl AnalyticsSink for CaptureSink {
    async fn log_event(&self, name: &str, m: LogEventMetadata) {
        self.events.lock().unwrap().push((name.to_string(), m));
    }
    async fn log_event_async(&self, name: &str, m: LogEventMetadata) {
        self.log_event(name, m).await;
    }
    fn name(&self) -> &str {
        "capture"
    }
}

fn write_file(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
}

#[tokio::test]
async fn settings_merge_fixture_matches_implementation() {
    let f: Fixture = load_fixture("settings_merge");

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::env::set_var("HOME", &home);
    write_file(
        &home.join(".lingxi").join("settings.json"),
        &serde_json::to_string(&f.input_user_settings_json).unwrap(),
    );
    write_file(
        &tmp.path()
            .join("project")
            .join(".lingxi")
            .join("settings.json"),
        &serde_json::to_string(&f.input_project_settings_json).unwrap(),
    );

    let defaults: SettingsJson = serde_json::from_value(f.input_defaults.clone()).unwrap();

    let bus = Arc::new(AnalyticsBus::new());
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
        .await;

    let eff = Settings::load_with_observer(
        LoadInputs {
            env: &f.input_env,
            project_dir: &tmp.path().join("project"),
            defaults,
        },
        Some(bus.as_ref()),
    )
    .await
    .unwrap();

    // Effective settings: compare via Value round-trip to ignore ordering.
    let actual_value = serde_json::to_value(&eff.settings).unwrap();
    let actual_obj = actual_value
        .as_object()
        .expect("merged settings serializes as object");
    let expected_obj = f
        .expected_effective_settings
        .as_object()
        .expect("fixture expected is an object");
    for (k, v_expected) in expected_obj {
        let v_actual = actual_obj
            .get(k)
            .unwrap_or_else(|| panic!("missing field {k}"));
        assert_eq!(v_actual, v_expected, "mismatch on field {k}");
    }

    // Provenance: every expected field has the expected ordered list of sources.
    for (field, expected_sources) in &f.expected_provenance {
        let prov = eff
            .effective_for(field)
            .unwrap_or_else(|| panic!("missing provenance for {field}"));
        let actual: Vec<String> = prov
            .contributors
            .iter()
            .map(|s| format!("{s:?}").to_lowercase())
            .collect();
        assert_eq!(&actual, expected_sources, "provenance mismatch for {field}");
    }

    // Telemetry: locked event names + locked keys.
    let captured = sink.events.lock().unwrap().clone();
    let names: Vec<String> = captured.iter().map(|(n, _)| n.clone()).collect();
    for expected in &f.expected_telemetry_event_names {
        assert!(
            names.contains(expected),
            "expected event {expected}, got {names:?}"
        );
    }
    let loaded = captured
        .iter()
        .find(|(n, _)| n == "tengu_settings_loaded")
        .unwrap();
    for key in &f.expected_telemetry_loaded_keys {
        assert!(
            loaded.1.contains_key(key),
            "tengu_settings_loaded missing key {key}"
        );
    }
    // Sanity: layers_present and had_env_override carry the right value types.
    assert!(matches!(
        loaded.1.get("layers_present"),
        Some(AnalyticsValue::Int(_))
    ));
    assert!(matches!(
        loaded.1.get("had_env_override"),
        Some(AnalyticsValue::Bool(_))
    ));
}
