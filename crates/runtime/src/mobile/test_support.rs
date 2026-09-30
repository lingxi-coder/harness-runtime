//! Host-only walking-skeleton support (plan F3-06).
//!
//! The real device `Platform` (`platform-ios` / `platform-android`) is
//! `cfg(target_os)`-gated in `Cargo.toml`, so off-device `build_mobile_engine`
//! cannot construct one. This module supplies the portable stand-ins that let
//! the shared session host be built and driven on CI WITHOUT a device:
//!
//! - [`HostFakePlatform`] — an `Arc<dyn Platform>` backed entirely by the
//!   portable `platform-posix-minimal` handles (`std::fs` over a temp root,
//!   `std::time`, stubbed http/process/sandbox/worktree, no camera/voice/share).
//! - [`FakeListener`] — a recording [`ClientEventListener`], the off-device
//!   stand-in for a Swift/Kotlin listener (`received` is public so a test reads
//!   back the delivered [`ClientEvent`]s).
//! - [`CollectingPermissionSink`] — a [`PermissionRequestSink`] that records the
//!   gate's outbound requests.
//! - [`new_engine_with_streaming`] — builds a real [`MobileEngineHandle`] off
//!   device with a caller-supplied streaming client (a scripted
//!   [`orchestrator::test_support_stream::MockStreamingApiClient`] stands in for
//!   the network), so `submit(SendPrompt)` drives a deterministic turn.
//!
//! These are shared by both the in-crate F3-03/F3-05 unit tests and the
//! `tests/skeleton_test.rs` integration test so there is ONE off-device host
//! definition (no drift). Behind the `uniffi` feature — they name the FFI-surface
//! types.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use client::adapter::{ClientEventListener, PermissionRequestSink};
use client::protocol::events::ClientEvent;
use client::protocol::permission::PermissionRequest as PermissionRequestDto;
use lingxi_core::host::{
    CameraControl, Clock, FileSystem, HttpTransport, Platform, ProcessRunner, Sandbox,
    SecureStorage, SharingService, WorktreeManager,
};
use orchestrator::StreamingApiClient;
use tokio::sync::Mutex;

pub use crate::mobile::host::{
    build_mobile_engine_inner, MobileConfig, MobileEngineError, MobileEngineHandle,
};

/// Off-device fake [`Platform`] shim: reuses the portable
/// `platform-posix-minimal` handles (`std::fs` over a temp root, `std::time`,
/// stubbed http/process/sandbox/worktree, no device capabilities) so the shared
/// session host is built and driven on CI without a device.
pub struct HostFakePlatform {
    fs: Arc<dyn FileSystem>,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    process: Arc<dyn ProcessRunner>,
    sandbox: Arc<dyn Sandbox>,
    worktree: Arc<dyn WorktreeManager>,
    secure_storage: Option<Arc<dyn SecureStorage>>,
}

impl HostFakePlatform {
    /// Construct a host fake rooted at `root` (the temp `cwd`/`lingxi_home`).
    #[must_use]
    pub fn new(root: std::path::PathBuf) -> Self {
        use platform_posix_minimal::{
            PosixClock, PosixFileSystem, PosixHttp, PosixProcess, PosixSandbox, PosixWorktree,
        };
        Self {
            fs: Arc::new(PosixFileSystem::new(root)),
            http: Arc::new(PosixHttp::new()),
            clock: Arc::new(PosixClock::new()),
            process: Arc::new(PosixProcess::new()),
            sandbox: Arc::new(PosixSandbox::new()),
            worktree: Arc::new(PosixWorktree::new()),
            secure_storage: None,
        }
    }

    /// Inject a fake secure store so the built runtime reports
    /// `oauth_supported = store.is_encrypted()` — used to prove the native
    /// secure-storage injection seam end-to-end off-device.
    #[must_use]
    pub fn with_secure_storage(mut self, store: Arc<dyn SecureStorage>) -> Self {
        self.secure_storage = Some(store);
        self
    }
}

impl Platform for HostFakePlatform {
    fn filesystem(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }
    fn http(&self) -> Arc<dyn HttpTransport> {
        self.http.clone()
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    fn process(&self) -> Arc<dyn ProcessRunner> {
        self.process.clone()
    }
    fn sandbox(&self) -> Arc<dyn Sandbox> {
        self.sandbox.clone()
    }
    fn worktree(&self) -> Arc<dyn WorktreeManager> {
        self.worktree.clone()
    }
    // camera / audio / share default to `None` — the host shim has no device
    // capabilities, exactly the off-device contract.
    fn camera(&self) -> Option<Arc<dyn CameraControl>> {
        None
    }
    fn share(&self) -> Option<Arc<dyn SharingService>> {
        None
    }
    fn secure_storage(&self) -> Option<Arc<dyn SecureStorage>> {
        self.secure_storage.clone()
    }
}

/// A host-fake [`ClientEventListener`] that records every delivered event — the
/// off-device stand-in for a Swift/Kotlin listener.
#[derive(Default)]
pub struct FakeListener {
    /// Every [`ClientEvent`] the adapter delivered, in arrival order.
    pub received: Mutex<Vec<ClientEvent>>,
    /// Every direct structured workflow update, in arrival order.
    pub workflow_progress: Mutex<
        Vec<(
            String,
            String,
            String,
            client::protocol::listings::WorkflowProgressDto,
        )>,
    >,
}

#[async_trait]
impl ClientEventListener for FakeListener {
    async fn on_event(&self, event: ClientEvent) {
        self.received.lock().await.push(event);
    }

    async fn on_workflow_progress(
        &self,
        origin_session_id: String,
        task_id: String,
        run_id: String,
        progress: client::protocol::listings::WorkflowProgressDto,
    ) {
        self.workflow_progress
            .lock()
            .await
            .push((origin_session_id, task_id, run_id, progress));
    }
}

/// A [`PermissionRequestSink`] that counts the gate's outbound requests so a test
/// can prove `check()` reached the adapter gate (not an always-allow no-op).
#[derive(Default)]
pub struct CollectingPermissionSink {
    /// Number of permission requests the gate emitted.
    pub count: AtomicUsize,
}

impl CollectingPermissionSink {
    /// Snapshot the emitted-request count.
    #[must_use]
    pub fn emitted(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PermissionRequestSink for CollectingPermissionSink {
    async fn emit_request(&self, _request: PermissionRequestDto) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

/// A `MobileConfig` rooted at `cwd` (`cwd` + `lingxi_home` under it), otherwise
/// the frozen defaults — the off-device build recipe the host tests share.
#[must_use]
pub fn test_config(cwd: &std::path::Path) -> MobileConfig {
    MobileConfig {
        cwd: cwd.to_path_buf(),
        lingxi_home: cwd.join(branding::DOT_DIR),
        ..MobileConfig::default()
    }
}

/// Build a real, fully-wired [`MobileEngineHandle`] off device with a
/// caller-supplied streaming client (plan F3-06 — the walking skeleton).
///
/// `streaming` is the stubbed [`StreamingApiClient`] the orchestrator drives for
/// its turn loop — a scripted
/// [`orchestrator::test_support_stream::MockStreamingApiClient`] stands in for
/// the network so `submit(SendPrompt)` produces a deterministic
/// `TextDelta`→`TurnEnded` sequence on the listener. `None` falls back to the
/// production router-backed adapter (which would attempt a real request via the
/// platform's HTTP transport).
///
/// # Errors
///
/// Returns [`MobileEngineError`] if the handle-owned tokio runtime or the shared
/// `build_mobile` cannot be constructed (effectively infallible off device).
pub fn new_engine_with_streaming(
    cfg: MobileConfig,
    platform: Arc<dyn Platform>,
    listener: Arc<dyn ClientEventListener>,
    permission_sink: Arc<dyn PermissionRequestSink>,
    streaming: Option<Arc<dyn StreamingApiClient>>,
) -> Result<Arc<MobileEngineHandle>, MobileEngineError> {
    build_mobile_engine_inner(cfg, platform, listener, permission_sink, streaming)
}
