//! Test-only helpers: process-env serialization + temp config dirs + a
//! capturing telemetry sink for emission-contract tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use telemetry::sink::{AnalyticsSink, LogEventMetadata};
use telemetry::AnalyticsBus;

/// Process-wide lock for tests that mutate env vars (`HOME`,
/// `LINGXI_CONFIG_DIR`, `DISABLE_AUTOUPDATER`, provider gates). Cargo runs
/// tests in parallel threads sharing the process env; hold this for the
/// test's whole body.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A throwaway config universe: `dir` is a tempdir acting as the Claude
/// config home; `global` is the `~/.lingxi.json`-equivalent path inside it.
pub struct TempConfig {
    /// Owns the tempdir (deleted on drop).
    pub _tmp: tempfile::TempDir,
    /// Stand-in for `~/.claude` (claude config home).
    pub home: PathBuf,
    /// Stand-in for `~/.lingxi.json` (global config file).
    pub global: PathBuf,
    /// Stand-in project directory (for settings.local.json).
    pub project: PathBuf,
}

/// Build a fresh [`TempConfig`]. No env mutation — APIs take explicit paths.
pub fn temp_config() -> TempConfig {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("claude-home");
    let global = tmp.path().join("claude.json");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&home).expect("mk home");
    std::fs::create_dir_all(&project).expect("mk project");
    TempConfig {
        _tmp: tmp,
        home,
        global,
        project,
    }
}

/// Captured `(event name, metadata-as-JSON)` pairs, shared with the test
/// body. Metadata is serialized to `serde_json::Value` because
/// `AnalyticsValue` has no `PartialEq`; `json!` comparisons are exact thanks
/// to its `#[serde(untagged)]` encoding.
pub type CapturedEvents = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// [`AnalyticsSink`] that records every event for assertion.
struct CapturingSink {
    events: CapturedEvents,
}

#[async_trait::async_trait]
impl AnalyticsSink for CapturingSink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        // std Mutex, never held across an await point.
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                name.to_string(),
                serde_json::to_value(&metadata).expect("AnalyticsValue serializes"),
            ));
    }

    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "capturing-test-sink"
    }
}

/// Build an [`AnalyticsBus`] with a capturing sink already attached (so
/// events are delivered immediately, not buffered) plus the shared capture
/// vector for assertions. Wire the bus into `MigrationEnv::bus`.
pub async fn capture_bus() -> (Arc<AnalyticsBus>, CapturedEvents) {
    let events: CapturedEvents = Arc::default();
    let bus = AnalyticsBus::new();
    bus.attach_sink(Arc::new(CapturingSink {
        events: events.clone(),
    }))
    .await;
    (Arc::new(bus), events)
}
