//! Shared test helpers for builtin tool unit tests.
//!
//! Cleanly avoids re-declaring a 100-line `PanickingFs` impl in every tool's
//! test module. The 6 M4-01 tools all use `tokio::fs` directly; the
//! `Arc<dyn FileSystem>` field in `BuiltinToolContext` is required for future
//! M5 sandbox wiring but never touched by the tools themselves.

use crate::context::{ToolUseContext, ToolUseOptions};
use crate::progress::{progress_channel, ToolProgressSender};
use async_trait::async_trait;
use std::sync::Arc;

/// A stub `FileSystem` that panics on general-purpose methods.
///
/// Security-sensitive root-confined methods use the production on-disk helper
/// so cron/workflow-style tests exercise real no-follow/atomic behavior. Other
/// methods remain fail-fast to expose accidental test dependencies.
pub struct PanickingFs;

#[async_trait]
impl lingxi_core::host::filesystem::FileSystem for PanickingFs {
    async fn read_file(
        &self,
        _: &str,
        _: Option<u64>,
        _: Option<u64>,
    ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::filesystem::FsError>
    {
        panic!("M4-01 builtin tools do not call FileSystem::read_file");
    }
    async fn write_file(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("M4-01 builtin tools do not call FileSystem::write_file")
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = lingxi_core::host::filesystem::FileEvent> + Send>,
        >,
        lingxi_core::host::filesystem::FsError,
    > {
        panic!("not called")
    }
    async fn append_file(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn truncate(
        &self,
        _: &str,
        _: u64,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn file_mtime(
        &self,
        _: &str,
    ) -> Result<std::time::SystemTime, lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn file_size(&self, _: &str) -> Result<u64, lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn delete_file(&self, _: &str) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn symlink(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn flock_exclusive(
        &self,
        _: &str,
    ) -> Result<
        Box<dyn lingxi_core::host::filesystem::FlockGuard>,
        lingxi_core::host::filesystem::FsError,
    > {
        panic!("not called")
    }
    async fn fsync(&self, _: &str) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn read_file_rooted_no_follow(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
    ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::filesystem::FsError>
    {
        let content = lingxi_core::host::rooted_fs::read_to_string(root, relative)?;
        Ok(lingxi_core::host::filesystem::FileContent {
            total_lines: content.lines().count() as u64,
            content,
            truncated: false,
        })
    }
    async fn write_file_rooted_atomic(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
        content: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        lingxi_core::host::rooted_fs::atomic_write(
            root,
            relative,
            content.as_bytes(),
            lingxi_core::host::AtomicWriteOptions::default(),
        )
    }
    async fn flock_exclusive_rooted(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
    ) -> Result<
        Box<dyn lingxi_core::host::filesystem::FlockGuard>,
        lingxi_core::host::filesystem::FsError,
    > {
        lingxi_core::host::rooted_fs::lock_exclusive(
            root,
            relative,
            lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE,
            lingxi_core::host::rooted_fs::PRIVATE_FILE_MODE,
        )
        .map(|guard| Box::new(guard) as Box<dyn lingxi_core::host::filesystem::FlockGuard>)
    }
    async fn delete_file_rooted_no_follow(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        lingxi_core::host::rooted_fs::remove_file(root, relative)
    }
}

/// Convenience: return a `PanickingFs` wrapped as `Arc<dyn FileSystem>`.
#[must_use]
pub fn make_dummy_fs() -> Arc<dyn lingxi_core::host::filesystem::FileSystem> {
    Arc::new(PanickingFs) as _
}

/// Test filesystem whose ONLY live behavior is `translate_model_path`:
/// `guest_prefix/…` maps onto `host_prefix/…` and anything under
/// `fence_prefix` is refused — the mobile-linux guest-path contract the file
/// tools consult before canonicalization. Every I/O method panics like
/// [`PanickingFs`], which doubles as proof that the tools still run their I/O
/// on raw `tokio::fs`, never through this trait.
struct GuestAliasFs {
    guest_prefix: String,
    host_prefix: std::path::PathBuf,
    fence_prefix: String,
}

#[async_trait]
impl lingxi_core::host::filesystem::FileSystem for GuestAliasFs {
    async fn read_file(
        &self,
        _: &str,
        _: Option<u64>,
        _: Option<u64>,
    ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::filesystem::FsError>
    {
        panic!("file tools run on raw tokio::fs, not FileSystem::read_file")
    }
    async fn write_file(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("file tools run on raw tokio::fs, not FileSystem::write_file")
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = lingxi_core::host::filesystem::FileEvent> + Send>,
        >,
        lingxi_core::host::filesystem::FsError,
    > {
        panic!("not called")
    }
    async fn append_file(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn truncate(
        &self,
        _: &str,
        _: u64,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn file_mtime(
        &self,
        _: &str,
    ) -> Result<std::time::SystemTime, lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn file_size(&self, _: &str) -> Result<u64, lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn delete_file(&self, _: &str) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn symlink(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    async fn flock_exclusive(
        &self,
        _: &str,
    ) -> Result<
        Box<dyn lingxi_core::host::filesystem::FlockGuard>,
        lingxi_core::host::filesystem::FsError,
    > {
        panic!("not called")
    }
    async fn fsync(&self, _: &str) -> Result<(), lingxi_core::host::filesystem::FsError> {
        panic!("not called")
    }
    fn translate_model_path(
        &self,
        path: &str,
        _write: bool,
    ) -> Result<Option<String>, lingxi_core::host::filesystem::FsError> {
        if path == self.fence_prefix
            || path
                .strip_prefix(&self.fence_prefix)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return Err(lingxi_core::host::filesystem::FsError::PermissionDenied(
                format!("guest path is not host-backed: {path}"),
            ));
        }
        if let Some(rest) = path.strip_prefix(&self.guest_prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                return Ok(Some(format!(
                    "{}{rest}",
                    self.host_prefix.to_string_lossy()
                )));
            }
        }
        Ok(None)
    }
}

/// Build a [`GuestAliasFs`] as `Arc<dyn FileSystem>` — see its docs.
#[must_use]
pub fn make_guest_alias_fs(
    guest_prefix: &str,
    host_prefix: &std::path::Path,
    fence_prefix: &str,
) -> Arc<dyn lingxi_core::host::filesystem::FileSystem> {
    Arc::new(GuestAliasFs {
        guest_prefix: guest_prefix.to_string(),
        host_prefix: host_prefix.to_path_buf(),
        fence_prefix: fence_prefix.to_string(),
    }) as _
}

/// Process-wide HOME lock for tests that mutate `$HOME` via `std::env::set_var`.
///
/// M4-08 builtin tools (Brief, Config, CronCreate, RemoteTrigger) each
/// resolve their on-disk dir from `$HOME`. Their unit tests redirect HOME to
/// a `tempfile::TempDir`; serialize them through this single lock so parallel
/// test threads from different modules don't race on the same env var.
///
/// `tokio::sync::Mutex` (not `std::sync::Mutex`) so the guard can be safely
/// held across `await` points in `#[tokio::test]` bodies.
pub static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Build a fresh, minimal [`ToolUseContext`] for unit tests.
#[must_use]
pub fn fresh_ctx() -> ToolUseContext {
    ToolUseContext {
        options: ToolUseOptions {
            debug: false,
            verbose: false,
            main_loop_model: "test".into(),
            model_profile: None,
            max_budget_nano_usd: None,
            mcp_clients: vec![],
            is_non_interactive_session: false,
            custom_system_prompt: None,
            append_system_prompt: None,
        },
        messages: vec![],
        tool_use_id: None,
        assistant_message_id: None,
        agent_id: None,
        agent_name: None,
        team_name: None,
        origin_session_id: None,
        tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
        content_replacement_state: None,
        session: None,
        subagent_registry: None,
        cancel: None,
        fork_parent_system_prompt: None,
        cwd: None,
        depth: 0,
        observer: None,
        observer_pairings: None,
        file_history: None,
    }
}

/// Build a fresh, minimal [`ToolUseContext`] whose per-call `cancel` token has
/// ALREADY been triggered. Lets a test exercise a tool's mid-flight
/// cancellation path (e.g. `TaskOutput`'s blocking wait loop) without taking a
/// direct `tokio-util` dependency in the tool crate.
#[must_use]
pub fn fresh_ctx_cancelled() -> ToolUseContext {
    let mut ctx = fresh_ctx();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    ctx.cancel = Some(token);
    ctx
}

/// Build a fresh progress sender wired to a dropped receiver.
#[must_use]
pub fn fresh_tx() -> ToolProgressSender {
    let (tx, _rx) = progress_channel();
    tx
}

// ===== M4-02 shell-tool test stubs ==========================================

use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
use lingxi_core::host::sandbox::{
    ProcessCommand as SbxCommand, Sandbox, SandboxCapability, SandboxError, SandboxFeatures,
    SandboxPolicy, SandboxedCommand, SandboxedTag,
};
use mobile_linux_api::SandboxBackend;
use mobile_linux_api::{ProcessError, ProcessOutput};
use std::sync::Mutex;

/// In-test `ProcessRunner` that returns a canned [`ProcessOutput`] for each
/// `run` call. Panics on `spawn_background` (most tests don't need it; the
/// bash background test wires up its own bespoke stub).
pub struct StubProcess {
    queued: Mutex<Vec<ProcessOutput>>,
}

impl StubProcess {
    /// Construct a stub that will return `outputs[i]` on the i-th `run` call.
    #[must_use]
    pub fn with(outputs: Vec<ProcessOutput>) -> Self {
        Self {
            queued: Mutex::new(outputs),
        }
    }

    /// Single-output convenience constructor.
    #[must_use]
    pub fn single(output: ProcessOutput) -> Self {
        Self::with(vec![output])
    }

    /// Number of queued outputs not yet consumed by a `run` call. Lets a test
    /// assert HOW MANY process runs happened (e.g. that a cached shell snapshot
    /// runs once, not per call).
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.queued.lock().unwrap().len()
    }
}

#[async_trait]
impl ProcessRunner for StubProcess {
    async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        let mut q = self.queued.lock().unwrap();
        if q.is_empty() {
            return Err(ProcessError::Io("stub exhausted".into()));
        }
        Ok(q.remove(0))
    }

    async fn spawn_background(
        &self,
        _cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// Auditing-bypass-only `Sandbox` impl. Every command is tagged as a bypass
/// with the supplied reason — convenient for unit tests that don't care
/// about sandbox semantics.
pub struct BypassSandbox;

#[async_trait]
impl Sandbox for BypassSandbox {
    fn is_available(&self) -> bool {
        true
    }
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::None
    }
    fn prepare(
        &self,
        cmd: SbxCommand,
        _policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError> {
        Ok(SandboxedCommand::__new_sandboxed(
            cmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: "test_bypass".into(),
            },
        ))
    }
    fn bypass_with_audit(&self, cmd: SbxCommand, reason: &str) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            cmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.into(),
            },
        )
    }
    async fn probe_capability(&self) -> SandboxCapability {
        SandboxCapability {
            available: true,
            reason: None,
            features: SandboxFeatures::default(),
        }
    }
}

/// Convenience: wrap [`StubProcess::single`] in `Arc<dyn ProcessRunner>`.
#[must_use]
pub fn make_stub_process(out: ProcessOutput) -> Arc<dyn ProcessRunner> {
    Arc::new(StubProcess::single(out))
}

/// Convenience: wrap [`BypassSandbox`] in `Arc<dyn Sandbox>`.
#[must_use]
pub fn make_bypass_sandbox() -> Arc<dyn Sandbox> {
    Arc::new(BypassSandbox)
}

/// Minimal in-test wall-clock that ticks deterministically.
pub struct StubClock {
    now: Mutex<std::time::SystemTime>,
}

impl StubClock {
    /// Construct anchored at the Unix epoch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: Mutex::new(std::time::UNIX_EPOCH),
        }
    }
}

impl Default for StubClock {
    fn default() -> Self {
        Self::new()
    }
}

impl lingxi_core::host::Clock for StubClock {
    fn now(&self) -> std::time::SystemTime {
        *self.now.lock().unwrap()
    }
}

/// Convenience: wrap a fresh `StubClock` in `Arc<dyn Clock>`.
#[must_use]
pub fn make_stub_clock() -> Arc<dyn lingxi_core::host::Clock> {
    Arc::new(StubClock::new())
}

/// A `HttpTransport` stub that always errors. Satisfies the
/// `BuiltinToolContext::http` field for tests that don't exercise it.
pub struct PanickingHttp;

#[async_trait]
impl lingxi_core::host::http::HttpTransport for PanickingHttp {
    async fn request(
        &self,
        _: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::http::HttpError> {
        Err(lingxi_core::host::http::HttpError::InvalidRequest(
            "stub PanickingHttp: not configured for this test".into(),
        ))
    }
    async fn stream_sse(
        &self,
        _: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::http::HttpError> {
        Err(lingxi_core::host::http::HttpError::InvalidRequest(
            "stub PanickingHttp: stream_sse not supported".into(),
        ))
    }
}

/// Convenience: wrap [`PanickingHttp`] in `Arc<dyn HttpTransport>`.
#[must_use]
pub fn make_stub_http() -> Arc<dyn lingxi_core::host::http::HttpTransport> {
    Arc::new(PanickingHttp)
}

// ===== M4-04 workflow-tool test stubs =======================================

use lingxi_core::host::worktree::{
    WorktreeChangeSummary, WorktreeError, WorktreeHandle, WorktreeInfo, WorktreeManager,
};
use std::path::PathBuf;
use std::time::Duration;

/// In-memory `WorktreeManager` for hermetic tests. Tracks every call so tests
/// can assert on `created`, `removed`, `listed`. Reuses the M2-01 slug helpers
/// (`validate_worktree_slug` + `flatten_slug`) so its outputs are byte-aligned
/// with production.
#[allow(dead_code)] // M4-04 Tasks 10/11/13 use these helpers
#[derive(Default)]
pub struct MockWorktreeManager {
    inner: std::sync::Mutex<MockWtInner>,
}

#[derive(Default)]
struct MockWtInner {
    created: Vec<(String, WorktreeHandle)>,
    removed: Vec<WorktreeHandle>,
    entered_existing: Vec<PathBuf>,
    next_path_root: Option<PathBuf>,
    scripted_create_error: Option<WorktreeError>,
    scripted_remove_error: Option<WorktreeError>,
    scripted_enter_existing_error: Option<WorktreeError>,
    /// When set, `enter_existing` returns this EXACT handle (branch name
    /// verbatim, not derived from the path's final component) instead of the
    /// default synthesized one. Lets tests reproduce cases the derivation
    /// can't — e.g. a detached-HEAD worktree, whose `branch_name` is the
    /// literal `"HEAD"` (no `worktree-` prefix). Drained (`.take()`), like the
    /// scripted errors above.
    scripted_enter_existing_handle: Option<WorktreeHandle>,
    /// Deterministic dirty-state to return from `worktree_change_summary`.
    /// `None` (the default) → the trait default behavior (`Ok(None)`,
    /// fail-closed "unknown"). `Some(Some(..))` → that summary; `Some(None)`
    /// → an explicit fail-closed `Ok(None)`.
    /// The triple-state `Option<Option<_>>` is intentional (unset vs scripted
    /// `Some` vs scripted `None`), so the `option_option` lint is suppressed.
    #[allow(clippy::option_option)]
    scripted_change_summary: Option<Option<WorktreeChangeSummary>>,
}

#[allow(dead_code)] // M4-04 Tasks 10/11/13 use these helpers
impl MockWorktreeManager {
    /// Build a fresh `MockWorktreeManager`. Defaults to creating worktrees
    /// under `/tmp/mock-repo/.lingxi/worktrees/<flatten(slug)>`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the root used to assemble new worktree paths.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let s = Self::default();
        s.inner.lock().unwrap().next_path_root = Some(root.into());
        s
    }

    /// Force the next `create_worktree` call to return `err`.
    pub fn script_create_error(&self, err: WorktreeError) {
        self.inner.lock().unwrap().scripted_create_error = Some(err);
    }

    /// Force the next `remove_worktree` call to return `err`.
    #[allow(dead_code)]
    pub fn script_remove_error(&self, err: WorktreeError) {
        self.inner.lock().unwrap().scripted_remove_error = Some(err);
    }

    /// Force the next `enter_existing` call to return `err`.
    #[allow(dead_code)]
    pub fn script_enter_existing_error(&self, err: WorktreeError) {
        self.inner.lock().unwrap().scripted_enter_existing_error = Some(err);
    }

    /// Force the next `enter_existing` call to return `handle` verbatim
    /// (bypassing the default filename-derived branch name). Use this to
    /// reproduce a detached-HEAD worktree (`branch_name: "HEAD".into()`).
    #[allow(dead_code)]
    pub fn script_enter_existing_handle(&self, handle: WorktreeHandle) {
        self.inner.lock().unwrap().scripted_enter_existing_handle = Some(handle);
    }

    /// Script the dirty-state `worktree_change_summary` returns. Pass
    /// `Some(summary)` for a known state or `None` for fail-closed "unknown".
    /// Persistent (not drained) so a test can query before and after removal.
    #[allow(dead_code)]
    pub fn script_change_summary(&self, summary: Option<WorktreeChangeSummary>) {
        self.inner.lock().unwrap().scripted_change_summary = Some(summary);
    }

    /// Inspect created worktrees.
    #[must_use]
    pub fn created(&self) -> Vec<(String, WorktreeHandle)> {
        self.inner.lock().unwrap().created.clone()
    }

    /// Inspect removed worktrees.
    #[must_use]
    pub fn removed(&self) -> Vec<WorktreeHandle> {
        self.inner.lock().unwrap().removed.clone()
    }

    /// Inspect paths passed to `enter_existing`.
    #[must_use]
    pub fn entered_existing(&self) -> Vec<PathBuf> {
        self.inner.lock().unwrap().entered_existing.clone()
    }
}

#[async_trait]
impl WorktreeManager for MockWorktreeManager {
    async fn create_worktree(
        &self,
        slug: &str,
        _base_branch: Option<&str>,
        _copy_includes: &[PathBuf],
    ) -> Result<WorktreeHandle, WorktreeError> {
        // Drain scripted error first.
        if let Some(err) = self.inner.lock().unwrap().scripted_create_error.take() {
            return Err(err);
        }
        // Mirror the M2-01 validation locally — keeps the mock byte-aligned
        // with `EnterWorktreeTool`'s pre-flight check without pulling in
        // `lingxi-platform-posix` (which would form a dep cycle).
        validate_slug_inline(slug)?;
        let flat = flatten_slug_inline(slug);
        let root = self
            .inner
            .lock()
            .unwrap()
            .next_path_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/mock-repo"));
        let path = root.join(".lingxi").join("worktrees").join(&flat);
        let handle = WorktreeHandle {
            path,
            branch_name: format!("worktree-{flat}"),
            base_commit: None,
        };
        self.inner
            .lock()
            .unwrap()
            .created
            .push((slug.to_string(), handle.clone()));
        Ok(handle)
    }

    async fn remove_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError> {
        if let Some(err) = self.inner.lock().unwrap().scripted_remove_error.take() {
            return Err(err);
        }
        self.inner.lock().unwrap().removed.push(handle.clone());
        Ok(())
    }

    async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        Ok(Vec::new())
    }

    async fn cleanup_stale(&self, _max_age: Duration) -> Result<Vec<PathBuf>, WorktreeError> {
        Ok(Vec::new())
    }

    fn is_supported(&self) -> bool {
        true
    }

    async fn worktree_change_summary(
        &self,
        _handle: &WorktreeHandle,
    ) -> Result<Option<WorktreeChangeSummary>, WorktreeError> {
        // Return the scripted state if one was set; otherwise fall back to
        // the trait default ("unknown"). Not drained, so repeated queries
        // (e.g. an exit flow that checks then removes) stay consistent.
        Ok(self
            .inner
            .lock()
            .unwrap()
            .scripted_change_summary
            .unwrap_or(None))
    }

    async fn enter_existing(
        &self,
        path: &std::path::Path,
    ) -> Result<WorktreeHandle, WorktreeError> {
        if let Some(err) = self
            .inner
            .lock()
            .unwrap()
            .scripted_enter_existing_error
            .take()
        {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .entered_existing
            .push(path.to_path_buf());
        if let Some(handle) = self
            .inner
            .lock()
            .unwrap()
            .scripted_enter_existing_handle
            .take()
        {
            return Ok(handle);
        }
        // Deterministic handle: derive a stable branch name from the final
        // path component so tests can assert on it without depending on git.
        let branch_name = format!(
            "worktree-{}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        Ok(WorktreeHandle {
            path: path.to_path_buf(),
            branch_name,
            base_commit: None,
        })
    }
}

/// Convenience: wrap a fresh `MockWorktreeManager` in `Arc<dyn WorktreeManager>`.
#[must_use]
pub fn make_mock_worktree() -> Arc<dyn WorktreeManager> {
    Arc::new(MockWorktreeManager::new())
}

const MAX_SLUG_LEN: usize = 64;

fn validate_slug_inline(slug: &str) -> Result<(), WorktreeError> {
    if slug.is_empty() {
        return Err(WorktreeError::InvalidSlug("slug is empty".into()));
    }
    if slug.len() > MAX_SLUG_LEN {
        return Err(WorktreeError::InvalidSlug(format!(
            "slug exceeds {MAX_SLUG_LEN} chars (got {})",
            slug.len()
        )));
    }
    for segment in slug.split('/') {
        if segment.is_empty() {
            return Err(WorktreeError::InvalidSlug(format!(
                "slug contains empty segment: {slug:?}"
            )));
        }
        for ch in segment.chars() {
            let allowed = ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-';
            if !allowed {
                return Err(WorktreeError::InvalidSlug(format!(
                    "slug contains invalid character {ch:?} in segment {segment:?}"
                )));
            }
        }
    }
    Ok(())
}

fn flatten_slug_inline(slug: &str) -> String {
    slug.replace('/', "+")
}

/// Build a [`super::BuiltinToolContext`] for M4-01 file-tool unit tests
/// (process/sandbox/clock get stubbed defaults so the M4-02 fields satisfy
/// the struct shape without affecting file-tool behavior).
#[must_use]
pub fn ctx_for_file_tools(
    fs: Arc<dyn lingxi_core::host::filesystem::FileSystem>,
    bus: Arc<telemetry::AnalyticsBus>,
    trusted_dirs: Vec<std::path::PathBuf>,
) -> super::BuiltinToolContext {
    use permission::PermissionMode;
    use sandbox::runtime_config::{Platform, SandboxRuntimeConfig};

    let workspace = trusted_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));

    super::BuiltinToolContext {
        // No session: this context never persists tool output.
        session_id: None,
        fs,
        bus,
        process: make_stub_process(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }),
        sandbox: make_bypass_sandbox(),
        clock: make_stub_clock(),
        sandbox_runtime: SandboxRuntimeConfig::default(),
        sandbox_enabled_override: None,
        skip_web_fetch_preflight: false,
        ask_user_question_timeout: None,
        sandbox_runner: crate::sandbox_runner::default_sandbox_runner(),
        permission_mode: PermissionMode::Default,
        permission_policy: Arc::new(permission::PermissionPolicy::new(PermissionMode::Default)),
        sandbox_available: false,
        session_cwd: crate::session_cwd::SessionCwd::new(workspace, trusted_dirs),
        worktree_session: crate::worktree_session::new_worktree_session_cell(),
        platform: if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Linux
        },
        http: make_stub_http(),
        hosted_search: None,
        mcp_token_counter: None,
        default_model: "claude-sonnet-4-20250514".to_string(),
        web_search_config: None,
        worktree: make_mock_worktree(),
        subagent_spawner: None,
        agent_name_registry: None,
        task_registry: None,
        mailbox_router: None,
        budget_enforcer: None,
        main_loop_model_profile_provider: None,
        coordinator_mode: None,
        permission_gate: None,
        bash_edit_diff: None,
        mcp_registry: None,
        lsp_registry: None,
        camera: None,
        audio: None,
        audio_recording_handles: Arc::new(
            tokio::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        share: None,
        notifications: None,
        clipboard: None,
        computer_control: None,
        read_file_state: crate::read_file_state::new_read_file_state_map(),
        read_deny_exclude_globs: vec![],
        android_shell: None,
        android_git: None,
        android_git_secret: None,
        task_lifecycle_hooks: None,
    }
}

/// Build a full [`super::BuiltinToolContext`] for shell-tool unit tests.
#[must_use]
#[allow(dead_code)] // used by M4-02 shell-tool tests (bash/powershell/repl/sleep)
pub fn shell_test_ctx(out: ProcessOutput) -> super::BuiltinToolContext {
    use permission::PermissionMode;
    use sandbox::runtime_config::{Platform, SandboxRuntimeConfig};
    use std::path::PathBuf;
    use telemetry::AnalyticsBus;

    super::BuiltinToolContext {
        // No session: this context never persists tool output.
        session_id: None,
        fs: make_dummy_fs(),
        bus: Arc::new(AnalyticsBus::new()),
        process: make_stub_process(out),
        sandbox: make_bypass_sandbox(),
        clock: make_stub_clock(),
        sandbox_runtime: SandboxRuntimeConfig::default(),
        sandbox_enabled_override: None,
        skip_web_fetch_preflight: false,
        ask_user_question_timeout: None,
        sandbox_runner: crate::sandbox_runner::default_sandbox_runner(),
        permission_mode: PermissionMode::Default,
        permission_policy: Arc::new(permission::PermissionPolicy::new(PermissionMode::Default)),
        sandbox_available: false,
        session_cwd: crate::session_cwd::SessionCwd::new(
            PathBuf::from("/tmp"),
            vec![PathBuf::from("/tmp")],
        ),
        worktree_session: crate::worktree_session::new_worktree_session_cell(),
        platform: if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Linux
        },
        http: make_stub_http(),
        hosted_search: None,
        mcp_token_counter: None,
        default_model: "claude-sonnet-4-20250514".to_string(),
        web_search_config: None,
        worktree: make_mock_worktree(),
        subagent_spawner: None,
        agent_name_registry: None,
        task_registry: None,
        mailbox_router: None,
        budget_enforcer: None,
        main_loop_model_profile_provider: None,
        coordinator_mode: None,
        permission_gate: None,
        bash_edit_diff: None,
        mcp_registry: None,
        lsp_registry: None,
        camera: None,
        audio: None,
        audio_recording_handles: Arc::new(
            tokio::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        share: None,
        notifications: None,
        clipboard: None,
        computer_control: None,
        read_file_state: crate::read_file_state::new_read_file_state_map(),
        read_deny_exclude_globs: vec![],
        android_shell: None,
        android_git: None,
        android_git_secret: None,
        task_lifecycle_hooks: None,
    }
}

/// Like [`shell_test_ctx`] but with the project `workspace` pinned to
/// `workspace`. Used by the cron tools' tests, which key their single
/// `<workspace>/.lingxi/scheduled_tasks.json` persistence off the project root,
/// so each test can isolate it in its own tempdir.
#[must_use]
#[allow(dead_code)]
pub fn shell_test_ctx_in(
    out: ProcessOutput,
    workspace: std::path::PathBuf,
) -> super::BuiltinToolContext {
    let ctx = shell_test_ctx(out);
    let trusted = ctx.trusted_dirs();
    super::BuiltinToolContext {
        // No session: this context never persists tool output.
        session_id: None,
        session_cwd: crate::session_cwd::SessionCwd::new(workspace, trusted),
        ..ctx
    }
}

#[cfg(test)]
mod enter_existing_mock_tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn enter_existing_records_path_and_returns_deterministic_handle() {
        let mgr = MockWorktreeManager::new();
        let path = PathBuf::from("/tmp/mock-repo/.lingxi/worktrees/feature");
        let handle = mgr.enter_existing(&path).await.unwrap();
        assert_eq!(handle.path, path);
        assert_eq!(handle.branch_name, "worktree-feature");
        assert_eq!(handle.base_commit, None);
        assert_eq!(mgr.entered_existing(), vec![path]);
    }

    #[tokio::test]
    async fn enter_existing_honors_scripted_error() {
        let mgr = MockWorktreeManager::new();
        mgr.script_enter_existing_error(WorktreeError::Git("not a worktree".into()));
        let err = mgr
            .enter_existing(&PathBuf::from("/tmp/mock-repo/nope"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::Git(msg) if msg == "not a worktree"));
        // Scripted error is drained: no path recorded, and the next call
        // succeeds.
        assert!(mgr.entered_existing().is_empty());
        let path = PathBuf::from("/tmp/mock-repo/.lingxi/worktrees/again");
        let handle = mgr.enter_existing(&path).await.unwrap();
        assert_eq!(handle.path, path);
        assert_eq!(mgr.entered_existing(), vec![path]);
    }
}
