//! Shared plugin lifecycle telemetry without CLI dispatch dependencies.
use std::sync::Arc;
use telemetry::{AnalyticsBus, AnalyticsValue, LogEventMetadata};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginCommandOutcome {
    Success,
    Failure,
    Noop,
}

impl PluginCommandOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Noop => "noop",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PluginCommandTelemetry {
    pub scope: &'static str,
    pub json: bool,
    pub available: bool,
    pub all: bool,
    pub keep_data: bool,
    pub prune: bool,
    pub yes: bool,
    pub config_count: u64,
    pub installed_count: u64,
    pub available_count: u64,
    pub component_count: u64,
    pub removed_count: u64,
}

pub fn telemetry_scope(scope: Option<&str>) -> &'static str {
    match scope {
        Some("user") => "user",
        Some("project") => "project",
        Some("local") => "local",
        Some("managed") => "managed",
        Some(_) => "invalid",
        None => "auto",
    }
}

fn analytics_count(value: u64) -> AnalyticsValue {
    AnalyticsValue::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

pub fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread tokio runtime")
}

fn plugin_command_metadata(
    outcome: PluginCommandOutcome,
    telemetry: PluginCommandTelemetry,
) -> LogEventMetadata {
    let mut metadata = LogEventMetadata::new();
    metadata.insert(
        "outcome".into(),
        AnalyticsValue::String(outcome.as_str().to_string()),
    );
    metadata.insert(
        "scope".into(),
        AnalyticsValue::String(telemetry.scope.to_string()),
    );
    metadata.insert("json".into(), AnalyticsValue::Bool(telemetry.json));
    metadata.insert(
        "available".into(),
        AnalyticsValue::Bool(telemetry.available),
    );
    metadata.insert("all".into(), AnalyticsValue::Bool(telemetry.all));
    metadata.insert(
        "keep_data".into(),
        AnalyticsValue::Bool(telemetry.keep_data),
    );
    metadata.insert("prune".into(), AnalyticsValue::Bool(telemetry.prune));
    metadata.insert("yes".into(), AnalyticsValue::Bool(telemetry.yes));
    metadata.insert(
        "config_count".into(),
        analytics_count(telemetry.config_count),
    );
    metadata.insert(
        "installed_count".into(),
        analytics_count(telemetry.installed_count),
    );
    metadata.insert(
        "available_count".into(),
        analytics_count(telemetry.available_count),
    );
    metadata.insert(
        "component_count".into(),
        analytics_count(telemetry.component_count),
    );
    metadata.insert(
        "removed_count".into(),
        analytics_count(telemetry.removed_count),
    );
    metadata
}

pub async fn emit_plugin_command(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    event: &'static str,
    outcome: PluginCommandOutcome,
    telemetry: PluginCommandTelemetry,
) {
    tracing::info!(
        event = event,
        outcome = outcome.as_str(),
        scope = telemetry.scope,
        json = telemetry.json,
        available = telemetry.available,
        all = telemetry.all,
        keep_data = telemetry.keep_data,
        prune = telemetry.prune,
        yes = telemetry.yes,
        config_count = telemetry.config_count,
        installed_count = telemetry.installed_count,
        available_count = telemetry.available_count,
        component_count = telemetry.component_count,
        removed_count = telemetry.removed_count,
    );
    if let Some(bus) = analytics_bus {
        bus.log_event(event, plugin_command_metadata(outcome, telemetry))
            .await;
    }
}

pub async fn emit_plugin_cli_result(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    event: &'static str,
    outcome: PluginCommandOutcome,
    scope: &'static str,
    count: u64,
) {
    tracing::info!(
        event = event,
        outcome = outcome.as_str(),
        scope = scope,
        count = count,
    );
    if let Some(bus) = analytics_bus {
        let mut metadata = LogEventMetadata::new();
        metadata.insert(
            "outcome".into(),
            AnalyticsValue::String(outcome.as_str().to_string()),
        );
        metadata.insert("scope".into(), AnalyticsValue::String(scope.to_string()));
        metadata.insert("count".into(), analytics_count(count));
        bus.log_event(event, metadata).await;
    }
}

pub async fn emit_plugin_command_failed(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    command: &'static str,
    scope: &'static str,
    error_kind: &'static str,
) {
    tracing::warn!(
        event = telemetry::tengu::plugin::COMMAND_FAILED,
        outcome = PluginCommandOutcome::Failure.as_str(),
        command = command,
        scope = scope,
        error_kind = error_kind,
    );
    if let Some(bus) = analytics_bus {
        let mut metadata = LogEventMetadata::new();
        metadata.insert(
            "outcome".into(),
            AnalyticsValue::String(PluginCommandOutcome::Failure.as_str().to_string()),
        );
        metadata.insert(
            "command".into(),
            AnalyticsValue::String(command.to_string()),
        );
        metadata.insert("scope".into(), AnalyticsValue::String(scope.to_string()));
        metadata.insert(
            "error_kind".into(),
            AnalyticsValue::String(error_kind.to_string()),
        );
        bus.log_event(telemetry::tengu::plugin::COMMAND_FAILED, metadata)
            .await;
    }
}

pub async fn emit_plugin_prune_cli(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    scope: &'static str,
    removed_count: u64,
) {
    tracing::info!(
        event = telemetry::tengu::plugin::PRUNE_CLI,
        outcome = PluginCommandOutcome::Success.as_str(),
        scope = scope,
        removed_count = removed_count,
    );
    if let Some(bus) = analytics_bus {
        let mut metadata = LogEventMetadata::new();
        metadata.insert(
            "outcome".into(),
            AnalyticsValue::String(PluginCommandOutcome::Success.as_str().to_string()),
        );
        metadata.insert("scope".into(), AnalyticsValue::String(scope.to_string()));
        metadata.insert("removed_count".into(), analytics_count(removed_count));
        bus.log_event(telemetry::tengu::plugin::PRUNE_CLI, metadata)
            .await;
    }
}

pub async fn emit_plugin_state_file_error(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    command: &'static str,
    operation: &'static str,
    error_kind: &'static str,
) {
    tracing::warn!(
        event = telemetry::tengu::plugin::STATE_FILE_ERROR,
        outcome = PluginCommandOutcome::Failure.as_str(),
        command = command,
        operation = operation,
        error_kind = error_kind,
    );
    if let Some(bus) = analytics_bus {
        let mut metadata = LogEventMetadata::new();
        metadata.insert(
            "outcome".into(),
            AnalyticsValue::String(PluginCommandOutcome::Failure.as_str().to_string()),
        );
        metadata.insert(
            "command".into(),
            AnalyticsValue::String(command.to_string()),
        );
        metadata.insert(
            "operation".into(),
            AnalyticsValue::String(operation.to_string()),
        );
        metadata.insert(
            "error_kind".into(),
            AnalyticsValue::String(error_kind.to_string()),
        );
        bus.log_event(telemetry::tengu::plugin::STATE_FILE_ERROR, metadata)
            .await;
    }
}

#[cfg(test)]
pub(crate) mod telemetry_test_support {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    use tracing_subscriber::layer::SubscriberExt;

    static CLI_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[derive(Clone, Default)]
    struct EventCapture {
        events: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    }

    impl EventCapture {
        fn records(&self) -> Vec<BTreeMap<String, String>> {
            self.events.lock().unwrap().clone()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor<'a>(&'a mut BTreeMap<String, String>);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }

                fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }

                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }

                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }

                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .entry(field.name().to_string())
                        .or_insert_with(|| format!("{value:?}"));
                }
            }

            let mut record = BTreeMap::new();
            event.record(&mut Visitor(&mut record));
            self.events.lock().unwrap().push(record);
        }
    }

    pub(crate) struct IsolatedCliEnv {
        _tmp: tempfile::TempDir,
        _guard: MutexGuard<'static, ()>,
        previous_config_dir: Option<OsString>,
        previous_cwd: PathBuf,
        pub home: PathBuf,
        pub cwd: PathBuf,
    }

    impl IsolatedCliEnv {
        pub fn new() -> Self {
            let guard = CLI_ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let cwd = tmp.path().join("project");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            let previous_config_dir = std::env::var_os(branding::CONFIG_DIR_ENV);
            let previous_cwd = std::env::current_dir().unwrap();
            std::env::set_var(branding::CONFIG_DIR_ENV, &home);
            std::env::set_current_dir(&cwd).unwrap();
            Self {
                _tmp: tmp,
                _guard: guard,
                previous_config_dir,
                previous_cwd,
                home,
                cwd,
            }
        }
    }

    impl Drop for IsolatedCliEnv {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous_cwd).unwrap();
            if let Some(previous) = self.previous_config_dir.as_ref() {
                std::env::set_var(branding::CONFIG_DIR_ENV, previous);
            } else {
                std::env::remove_var(branding::CONFIG_DIR_ENV);
            }
        }
    }

    pub(crate) fn capture_events<R>(f: impl FnOnce() -> R) -> (R, Vec<BTreeMap<String, String>>) {
        let capture = EventCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let result = tracing::subscriber::with_default(subscriber, || {
            // Callsite interest is global and may have been cached while no
            // subscriber was active by another concurrently running CLI
            // test. Rebuild only after this thread-local capture subscriber
            // is installed so the event macros cannot be skipped entirely.
            tracing::callsite::rebuild_interest_cache();
            f()
        });
        (result, capture.records())
    }

    pub(crate) fn event<'a>(
        events: &'a [BTreeMap<String, String>],
        event_name: &str,
    ) -> &'a BTreeMap<String, String> {
        events
            .iter()
            .find(|record| record.get("event").is_some_and(|event| event == event_name))
            .unwrap_or_else(|| panic!("missing telemetry event {event_name}: {events:?}"))
    }
}
