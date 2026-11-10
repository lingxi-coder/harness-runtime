//! Runtime compatibility helpers and opt-in test fixtures.
//!
//! Runtime consumers keep these hook, permission and fixed-memory APIs at their
//! existing paths. Scripted clients and recording mocks require `test-support`
//! outside this crate's unit tests.

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;

// ============================================================================
// HookExecutor + PermissionGate stubs (Task 8)
// ============================================================================
//
// These local traits will be renamespaced or replaced by M5-05 (real
// PermissionGate) and M5-06 (real 4-arm HookExecutor). M5-02 ships
// allow-all stubs against minimal trait surfaces so the orchestrator can
// be constructed in tests without dragging in the full hooks/permission
// machinery.

// M5-06 Task 14: the local `HookExecutor` trait that M5-02 introduced is
// replaced by the real `hooks::HookExecutorImpl`. We re-export the
// concrete type so existing imports (crate::test_support::HookExecutor)
// keep working as a type alias.
pub use hooks::HookExecutorImpl as HookExecutor;

/// Construct an empty `HookExecutorImpl` suitable for tests + the
/// orchestrator's "no hooks configured" path. The registry is empty so
/// `execute()` always returns a fresh `AggregateHookResult::default()`
/// without ever calling the supplied http/runtime stubs.
///
/// M5-06 Task 14: replaces the M5-02 `NoOpHookExecutor` unit struct so
/// the orchestrator can carry an `Arc<HookExecutorImpl>` instead of an
/// `Arc<dyn local::HookExecutor>` trait object.
#[must_use]
pub fn noop_hook_executor() -> Arc<hooks::HookExecutorImpl> {
    use hooks::registry::HookRegistry;

    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "noop hook executor — http arm is never called with an empty registry".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "noop hook executor — sse arm is never called".into(),
            ))
        }
    }

    struct UnusedRuntime;
    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal(
                "noop hook executor — runtime arm is never called".into(),
            ))
        }
        async fn sleep(&self, _duration: std::time::Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    let registry = Arc::new(tokio::sync::RwLock::new(HookRegistry::new()));
    let http: Arc<dyn lingxi_core::host::HttpTransport> = Arc::new(UnusedHttp);
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> = Arc::new(UnusedRuntime);
    Arc::new(hooks::HookExecutorImpl::new(registry, http, runtime))
}

// M5-06 Task 14: M5-02's `pub struct NoOpHookExecutor;` is gone — the
// orchestrator now carries `Arc<HookExecutorImpl>` directly. Call sites
// previously using `Arc::new(NoOpHookExecutor)` should now call
// `crate::test_support::noop_hook_executor()` (which returns the Arc
// directly).

// M5-05 Task 2: PermissionGate + PermissionDecision are promoted to
// lingxi-lingxi_core::host::permission_gate. We re-export them here so existing
// orchestrator imports (crate::test_support::PermissionGate, …) keep
// working unchanged.
pub use permission::gate::{
    PermissionDecision, PermissionDecisionSource, PermissionGate, PermissionResolution,
};

/// Allow-all permission gate. Always returns `Allow`.
///
/// **M5-05:** the trait surface moved to `lingxi-traits` but the impl
/// stays here for back-compat with M5-02 / M5-04 tests that import
/// `crate::test_support::NoOpPermissionGate`. Production composition chooses
/// between this no-op and [`permission::InteractivePromptingGate`] before
/// constructing the orchestrator; the separate
/// [`crate::OrchestratorConfig::interactive_session`] flag controls
/// prompt/request interactivity.
pub struct NoOpPermissionGate;

#[async_trait]
impl PermissionGate for NoOpPermissionGate {
    async fn check(&self, _tool_name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

// ============================================================================
// StaticMemoryProvider (Task 9)
// ============================================================================

/// Fixed memory provider used by runtime composition and prompt-wiring fixtures.
/// Returns the supplied `Vec<MemoryFile>` regardless of cwd.
pub struct StaticMemoryProvider {
    files: Vec<crate::prompt::MemoryFile>,
}

impl StaticMemoryProvider {
    /// Empty fixture — `load()` always returns `vec![]`.
    #[must_use]
    pub fn empty() -> Self {
        Self { files: Vec::new() }
    }

    /// Pre-loaded fixture — `load()` always returns the provided files.
    #[must_use]
    pub fn with_files(files: Vec<crate::prompt::MemoryFile>) -> Self {
        Self { files }
    }
}

#[async_trait]
impl crate::prompt::MemoryHierarchyProvider for StaticMemoryProvider {
    async fn load(&self, _cwd: &std::path::Path) -> Vec<crate::prompt::MemoryFile> {
        self.files.clone()
    }
    async fn load_conditional_rules(
        &self,
        cwd: &std::path::Path,
        trigger: &std::path::Path,
        mode: crate::prompt::memory_block::InstructionFilesMode,
    ) -> Vec<crate::prompt::MemoryFile> {
        self.files
            .iter()
            .filter(|file| {
                file.globs.is_some()
                    && (mode != crate::prompt::memory_block::InstructionFilesMode::ManagedOnly
                        || file.tier == memory::lingxi_md::LingxiMdTier::Managed)
                    && crate::prompt::conditional_rules::rule_matches_touched_file(
                        file, trigger, cwd,
                    )
            })
            .cloned()
            .collect()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[path = "test_support_mocks.rs"]
mod fixtures;

#[cfg(any(test, feature = "test-support"))]
pub use fixtures::*;

/// Serialize todo reminder tests that select V1/V2 through process variables.
#[cfg(test)]
pub(crate) static TODO_REMINDER_ENV_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());
