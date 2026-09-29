//! Analytics translation for settings-load diagnostics from the shared core.

use crate::{AnalyticsBus, AnalyticsValue, LogEventMetadata, PiiTagged, Verified};
use lingxi_core::settings::SettingsLoadObserver;
use std::path::Path;

#[async_trait::async_trait]
impl SettingsLoadObserver for AnalyticsBus {
    async fn invalid_env(&self, var: String, value: String) {
        let mut md = LogEventMetadata::new();
        md.insert(
            "var".into(),
            AnalyticsValue::String(Verified::assert_safe(var).as_str().to_string()),
        );
        md.insert(
            "_PROTO_value".into(),
            AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(value).into_inner()),
        );
        self.log_event("tengu_settings_invalid_env", md).await;
    }

    async fn parse_error(&self, path: &Path, class: &'static str) {
        let mut md = LogEventMetadata::new();
        md.insert(
            "_PROTO_path".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
            ),
        );
        md.insert(
            "error".into(),
            AnalyticsValue::String(Verified::assert_safe(class.into()).as_str().to_string()),
        );
        self.log_event("tengu_settings_parse_error", md).await;
    }

    async fn loaded(&self, layers_present: i64, had_env_override: bool, user_path: Option<&Path>) {
        let mut md = LogEventMetadata::new();
        md.insert("layers_present".into(), AnalyticsValue::Int(layers_present));
        md.insert(
            "had_env_override".into(),
            AnalyticsValue::Bool(had_env_override),
        );
        if let Some(path) = user_path {
            md.insert(
                "_PROTO_user_path".into(),
                AnalyticsValue::String(
                    PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
                ),
            );
        }
        self.log_event("tengu_settings_loaded", md).await;
    }
}

#[cfg(test)]
mod tests {
    use lingxi_core::settings::{schema, LoadInputs, Settings};
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::Mutex;
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    // We deliberately hold the `HOME_LOCK` std::sync::Mutex across `.await`
    // in these tests — it serializes the whole test against other HOME
    // mutators, which is the whole point. Switching to tokio's async Mutex
    // would break the non-async load_tests sharing the same lock.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn emits_tengu_settings_loaded_with_pii_routing() {
        use crate::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};
        use async_trait::async_trait;
        use std::sync::{Arc, Mutex};

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

        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let bus = Arc::new(AnalyticsBus::new());
        let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
        bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path().join("home_t10"));
        let mut env = std::collections::BTreeMap::new();
        env.insert("LINGXI_MODEL".to_string(), "opus".to_string());

        let _eff = Settings::load_with_observer(
            LoadInputs {
                env: &env,
                project_dir: tmp.path(),
                defaults: schema::SettingsJson::default(),
            },
            Some(bus.as_ref()),
        )
        .await
        .unwrap();

        let captured = sink.events.lock().unwrap().clone();
        let loaded = captured
            .iter()
            .find(|(n, _)| n == "tengu_settings_loaded")
            .expect("tengu_settings_loaded must be emitted");
        assert!(matches!(
            loaded.1.get("had_env_override"),
            Some(AnalyticsValue::Bool(true))
        ));
        assert!(
            loaded.1.contains_key("_PROTO_user_path"),
            "user path is PII; must be PROTO-routed"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn emits_tengu_settings_invalid_env_for_bad_bool() {
        use crate::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};
        use async_trait::async_trait;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct CaptureSink {
            events: Mutex<Vec<(String, LogEventMetadata)>>,
        }
        #[async_trait]
        impl AnalyticsSink for CaptureSink {
            async fn log_event(&self, n: &str, m: LogEventMetadata) {
                self.events.lock().unwrap().push((n.to_string(), m));
            }
            async fn log_event_async(&self, n: &str, m: LogEventMetadata) {
                self.log_event(n, m).await;
            }
            fn name(&self) -> &str {
                "capture"
            }
        }

        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let bus = Arc::new(AnalyticsBus::new());
        let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
        bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path().join("home_t10b"));
        let mut env = std::collections::BTreeMap::new();
        env.insert("LINGXI_TELEMETRY_ENABLED".to_string(), "yes".to_string());

        let _ = Settings::load_with_observer(
            LoadInputs {
                env: &env,
                project_dir: tmp.path(),
                defaults: schema::SettingsJson::default(),
            },
            Some(bus.as_ref()),
        )
        .await
        .unwrap();

        let captured = sink.events.lock().unwrap().clone();
        let invalid = captured
            .iter()
            .find(|(n, _)| n == "tengu_settings_invalid_env")
            .expect("tengu_settings_invalid_env must be emitted");
        assert!(matches!(
            invalid.1.get("var"),
            Some(AnalyticsValue::String(s)) if s == "LINGXI_TELEMETRY_ENABLED"
        ));
        assert!(invalid.1.contains_key("_PROTO_value"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn emits_tengu_settings_parse_error_on_malformed_project_json() {
        use crate::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};
        use async_trait::async_trait;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct CaptureSink {
            events: Mutex<Vec<(String, LogEventMetadata)>>,
        }
        #[async_trait]
        impl AnalyticsSink for CaptureSink {
            async fn log_event(&self, n: &str, m: LogEventMetadata) {
                self.events.lock().unwrap().push((n.to_string(), m));
            }
            async fn log_event_async(&self, n: &str, m: LogEventMetadata) {
                self.log_event(n, m).await;
            }
            fn name(&self) -> &str {
                "capture"
            }
        }

        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let bus = Arc::new(AnalyticsBus::new());
        let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
        bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path().join("home_t10c"));
        let project_subdir = tmp.path().join(".lingxi");
        std::fs::create_dir_all(&project_subdir).unwrap();
        // Malformed JSON in the project layer.
        let mut pf = std::fs::File::create(project_subdir.join("settings.json")).unwrap();
        writeln!(pf, "{{not-json").unwrap();

        let result = Settings::load_with_observer(
            LoadInputs {
                env: &BTreeMap::new(),
                project_dir: tmp.path(),
                defaults: schema::SettingsJson::default(),
            },
            Some(bus.as_ref()),
        )
        .await;
        // (review #2) A malformed file is SKIPPED, not fatal: the load still
        // succeeds (here yielding just the defaults, since the only configured
        // layer was the bad project file) — claude-code skips files with errors
        // and keeps merging the rest. The parse error is still surfaced via
        // telemetry.
        let effective = result.expect("a malformed file is skipped; the load still succeeds");
        assert_eq!(
            effective.settings,
            schema::SettingsJson::default(),
            "the skipped bad layer contributes nothing; defaults remain"
        );

        let captured = sink.events.lock().unwrap().clone();
        let parse_err = captured
            .iter()
            .find(|(n, _)| n == "tengu_settings_parse_error")
            .expect("tengu_settings_parse_error must still be emitted for the skipped file");
        assert!(parse_err.1.contains_key("_PROTO_path"));
        assert!(matches!(
            parse_err.1.get("error"),
            Some(AnalyticsValue::String(s)) if s == "parse"
        ));
    }
}
