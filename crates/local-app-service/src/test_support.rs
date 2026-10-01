//! Doubles for what a host hands the service, shared by the service's own
//! tests.
//!
//! Every seam in [`crate::host`] and [`crate::template_catalog`] has a double
//! here or next to the one test module that needs a special one. None of them
//! knows anything about a particular host: that is the point of the seams, and
//! what keeps these tests runnable without an engine.

use crate::broker::LocalAppsHostBroker;
use crate::host::{BuildExecutor, DiagnosticsProvider, HostEvent, HostEventSink};
use crate::publication::{Exposure, ManagedApp, ManagedRuntime, McpPublisher, Published};
use crate::template_catalog::PluginBundle;
use async_trait::async_trait;
use local_app_contracts::diagnostics::{DiagnosticsSettleStatus, FileDiagnostics};
use local_app_contracts::execution::{CommandOutcome, IsolatedCommand};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;

/// Keeps every event the service reports, in the order it reported them.
#[derive(Default)]
pub(crate) struct RecordingSink {
    events: Mutex<Vec<HostEvent>>,
}

impl RecordingSink {
    pub(crate) fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A snapshot of everything reported so far.
    pub(crate) async fn events(&self) -> Vec<HostEvent> {
        self.events.lock().await.clone()
    }

    /// Whether nothing has been reported yet.
    pub(crate) async fn is_empty(&self) -> bool {
        self.events.lock().await.is_empty()
    }
}

#[async_trait]
impl HostEventSink for RecordingSink {
    async fn emit(&self, event: HostEvent) {
        self.events.lock().await.push(event);
    }
}

/// For the tests that do not look at what the service reports.
pub(crate) struct DiscardSink;

#[async_trait]
impl HostEventSink for DiscardSink {
    async fn emit(&self, _event: HostEvent) {}
}

/// An executor for a host that cannot run commands: every run is refused, as a
/// real one refuses when its runtime is not there.
pub(crate) struct UnavailableExecutor;

#[async_trait]
impl BuildExecutor for UnavailableExecutor {
    async fn run(&self, _command: IsolatedCommand) -> Result<CommandOutcome, String> {
        Err("runtime unavailable: test runtime never executes node".into())
    }
}

/// Diagnostics a test dictates: what has settled and what is held.
pub(crate) struct FakeDiagnostics {
    pub(crate) settle: Option<DiagnosticsSettleStatus>,
    pub(crate) files: Vec<FileDiagnostics>,
}

#[async_trait]
impl DiagnosticsProvider for FakeDiagnostics {
    async fn settle(
        &self,
        _workspace: &Path,
        _timeout: Duration,
    ) -> Option<DiagnosticsSettleStatus> {
        self.settle
    }

    async fn latest(&self, _workspace: &Path) -> Vec<FileDiagnostics> {
        self.files.clone()
    }
}

/// A publisher that answers from a table the test fills in: what the host holds
/// for each app, and which conversations each app is exposed to. For the tests
/// that check what the service does *with* a host's answers; the publishing
/// calls themselves are accepted and ignored (see `RecordingPublisher` in the
/// broker tests for the ones that check what the service asks).
#[derive(Default)]
pub(crate) struct ScriptedPublisher {
    held: StdMutex<HashMap<String, Published>>,
    exposed: StdMutex<HashSet<(String, String)>>,
}

impl ScriptedPublisher {
    pub(crate) fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The host now holds `published` for `app_id`.
    pub(crate) fn holds(&self, app_id: &str, published: Published) {
        self.held
            .lock()
            .expect("held")
            .insert(app_id.to_string(), published);
    }

    /// `app_id` is exposed to `conversation_id`, so a call from it may begin.
    pub(crate) fn expose_to(&self, conversation_id: &str, app_id: &str) {
        self.exposed
            .lock()
            .expect("exposed")
            .insert((conversation_id.to_string(), app_id.to_string()));
    }
}

#[async_trait]
impl McpPublisher for ScriptedPublisher {
    fn available(&self) -> bool {
        true
    }

    async fn publish(&self, _app: &ManagedApp, _runtime: ManagedRuntime) -> Result<(), String> {
        Ok(())
    }

    async fn unregister(&self, _app_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn disconnect_app(&self, _app_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn disconnect_all(&self) -> Result<(), String> {
        Ok(())
    }

    async fn expose(
        &self,
        _conversation_id: &str,
        _app: &ManagedApp,
        _pin: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn unpin(&self, _conversation_id: &str, _app_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn published(&self, app_id: &str) -> Published {
        self.held
            .lock()
            .expect("held")
            .get(app_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn begin_call(&self, conversation_id: &str, app_id: &str) -> Result<(), String> {
        let exposed = self
            .exposed
            .lock()
            .expect("exposed")
            .contains(&(conversation_id.to_string(), app_id.to_string()));
        if exposed {
            Ok(())
        } else {
            Err(format!("{app_id} is not exposed to {conversation_id}"))
        }
    }

    async fn end_call(&self, _conversation_id: &str, _app_id: &str) {}

    async fn exposures(&self, _conversation_id: &str) -> Vec<Exposure> {
        Vec::new()
    }
}

/// The template catalog as the plugin bundle checks it in, under a fixed
/// bundle digest. The service trusts a bundle's catalog bytes and journals the
/// digest, so tests that exercise the catalog read the bytes the plugin ships.
pub(crate) struct CheckedInBundle;

impl PluginBundle for CheckedInBundle {
    fn catalog_bytes(&self) -> &[u8] {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/catalog.json"
        ))
    }

    fn bundle_sha256(&self) -> &str {
        "b0d1e5f0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c"
    }
}

/// The plugin workflow ids the host reserves for its own authority. The
/// service's prose must never tell the model to launch one of them: the host
/// authorizes the one workflow a build needs and refuses any other.
///
/// The registry these mirror is the host's (`local_app_plugin_binding` in the
/// runtime); the runtime holds this copy to it with a test of its own.
pub(crate) const RESERVED_WORKFLOW_IDS: [&str; 3] = [
    "lingxi-local-app:local-app-build",
    "lingxi-local-app:local-app-use-test",
    "lingxi-local-app:local-app-mcp-authoring",
];

/// A broker that reports to `sink` and builds through `executor`, with the
/// checked-in catalog attached as its plugin bundle.
pub(crate) fn broker_with_sink(
    root: PathBuf,
    sink: Arc<dyn HostEventSink>,
    executor: Option<Arc<dyn BuildExecutor>>,
    full_runtime: bool,
    runtime_root: Option<PathBuf>,
) -> Arc<LocalAppsHostBroker> {
    let broker = LocalAppsHostBroker::new(root, sink, executor, full_runtime, runtime_root);
    let _ = broker.attach_plugin_bundle(Arc::new(CheckedInBundle));
    broker
}
