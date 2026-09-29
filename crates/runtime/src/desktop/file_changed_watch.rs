//! File-changed watcher → `FileChanged` hook fire (parity: claude-code
//! `src/utils/hooks/fileChangedWatcher.ts`).
//!
//! claude-code resolves a set of watch paths from the user's `FileChanged` hook
//! config — each hook's `matcher` is a pipe-separated list of filenames to watch
//! in `cwd` (e.g. `".envrc|.env"`, `fileChangedWatcher.ts:48-65`) — opens a
//! chokidar watcher over them (`ignoreInitial`, `awaitWriteFinish`,
//! `:67-78`), and on every debounced `change` / `add` / `unlink` fires the
//! `FileChanged` hook with the path + the chokidar event name
//! (`handleFileEvent`, `:80-106` → `executeFileChangedHooks`,
//! `utils/hooks.ts:4278-4294`). The Rust port had no such watcher; this module
//! adds it at the desktop composition root, where the orchestrator
//! (`orch.hooks`), the `cwd`, and a `FileSystem` are all in scope.
//!
//! ## What this does (and does NOT do)
//! SCOPE is firing the hook only. This watcher resolves the watch paths from the
//! registered `FileChanged` hooks, watches the PARENT directories of those paths
//! via the in-tree `fs_watch` primitive ([`lingxi_core::host::FileSystem::watch`]), and —
//! for every changed path that is one of the resolved watch paths — fires
//! [`FileChangedFirer::fire`] best-effort with the chokidar-style event kind.
//!
//! The "dynamic watch paths" feedback loop (`fileChangedWatcher.ts:108-131`,
//! where a fired hook's `hookSpecificOutput.watchPaths` adds more paths and the
//! watcher restarts over the union) IS ported: [`FileChangedFirer::fire`]
//! returns the fired hooks' resolved `watchPaths`, the per-directory watch loop
//! forwards a non-empty set to the supervisor, and the supervisor folds them in
//! and restarts (`updateWatchPaths`, also exposed as
//! [`FileChangedWatcherHandle::update_watch_paths`]).
//!
//! The `onCwdChanged` watcher rebind (`:133-175`, function `g`) IS ported: a
//! mid-session `cd` that moves the persistent shell cwd is signalled from the
//! Bash tool through the orchestrator's `CwdChanged` firer into
//! [`FileChangedWatcherHandle::on_cwd_changed`] (via the
//! [`hooks::WatcherRebinder`] seam), which re-resolves the retained ORIGINAL
//! matchers against the new cwd, REPLACES the watch set (`r=x.watchPaths`, not a
//! union), and restarts — guarded against an unchanged cwd (`if(_===S)return`).
//! Only the pure watch-set rebind lives here: the `CwdChanged` HOOKS themselves
//! fire shell-side (the Bash tool's `CwdChanged` firer, claude-code's `hjc()` /
//! `E3r`), so this rebind NEVER re-fires them — the two halves of `g` split
//! across the shell tool and the watcher without double-firing.
//!
//! ## Directory-watch vs file-watch (chokidar parity)
//! chokidar watches the exact file paths; the in-tree primitive
//! ([`platforms/posix::watch_helper`]) watches a DIRECTORY recursively and
//! errors on a missing target. So this watcher watches the deduplicated PARENT
//! directories of the resolved file paths (so a file created after init is still
//! observed — matching chokidar's `add` event under `ignoreInitial`) and fires
//! ONLY for events whose path is in the resolved watch set — the same observable
//! result as chokidar's per-file watch.
//!
//! ## Change-kind mapping (chokidar event names)
//! The in-tree [`lingxi_core::host::FileEventKind`] maps onto the chokidar event strings
//! claude-code fires (`fileChangedWatcher.ts:75-77`):
//! - [`FileEventKind::Created`]  → `"add"`
//! - [`FileEventKind::Modified`] → `"change"`
//! - [`FileEventKind::Deleted`]  → `"unlink"`
//!
//! ## Lifecycle
//! [`FileChangedWatcher::spawn`] starts a single supervisor task (which owns one
//! per-directory watch loop for each watched directory) and returns a
//! [`FileChangedWatcherHandle`]. Explicit shutdown stops the supervisor and
//! joins every directory loop before returning. Dropping the last handle
//! retains an abort-on-drop fallback; the supervisor's [`JoinSet`] also aborts
//! its children if the supervisor is dropped or panics.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_core::Stream;
use hooks::file_changed_firer::{FileChangedFire, FileChangedFirer};
use lingxi_core::host::{FileEvent, FileEventKind, FileSystem};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

/// Map an in-tree [`FileEventKind`] onto the chokidar event-name string
/// claude-code fires (`fileChangedWatcher.ts:75-77`).
#[must_use]
fn kind_to_event(kind: FileEventKind) -> &'static str {
    match kind {
        FileEventKind::Created => "add",
        FileEventKind::Modified => "change",
        FileEventKind::Deleted => "unlink",
    }
}

/// Resolve the set of watch paths from the registered `FileChanged` hooks.
///
/// Mirrors claude-code's `resolveWatchPaths` (`fileChangedWatcher.ts:48-65`):
/// each `FileChanged` hook's `matcher` is a pipe-separated list of filenames;
/// each name is resolved against `cwd` unless it is already absolute. Returns a
/// deduplicated, sorted set (a hook with no matcher contributes nothing — the
/// dynamic-path channel claude-code also folds in is not ported).
#[must_use]
pub fn resolve_watch_paths(matchers: &[&str], cwd: &Path) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for m in matchers {
        for name in m.split('|').map(str::trim) {
            if name.is_empty() {
                continue;
            }
            let p = Path::new(name);
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(name)
            };
            set.insert(resolved);
        }
    }
    set.into_iter().collect()
}

/// The deduplicated parent directories of `paths` — the targets the in-tree
/// directory watcher actually watches (a file created later is still observed).
/// A path with no parent (e.g. a bare filename) contributes `cwd` as fallback.
#[must_use]
fn watch_dirs_for(paths: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for p in paths {
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                set.insert(parent.to_path_buf());
            }
            _ => {
                set.insert(cwd.to_path_buf());
            }
        }
    }
    set.into_iter().collect()
}

/// A control message to the watcher supervisor.
#[derive(Debug)]
enum WatcherControl {
    /// Fold these (already-resolved, absolute) paths into the watch set and, if
    /// any are new, restart the per-directory loops over the union — the
    /// `updateWatchPaths` seam (claude-code `fileChangedWatcher.ts` `f`, the
    /// dynamic-`watchPaths` feedback loop `:108-131` and the exposed interface
    /// method).
    AddWatchPaths(Vec<PathBuf>),
    /// The persistent shell cwd moved to `new_cwd`: re-resolve the ORIGINAL
    /// matchers against it, REPLACE the watch set, and restart — the
    /// watcher-rebind half of claude-code's `onCwdChanged` (`fileChangedWatcher.ts`
    /// function `g`: `t=S ... r=x.watchPaths ... if(o)m()`). The `CwdChanged`
    /// hooks are fired elsewhere (the Bash tool's `CwdChanged` firer), so this
    /// variant NEVER fires them — it is the pure watch-set rebind, guarded
    /// against an unchanged cwd (`if(_===S)return`) in the supervisor.
    CwdChanged { new_cwd: PathBuf },
    /// Replace the configuration-derived matcher set after a Desktop settings
    /// save. Unlike dynamic watch-path additions this is an authoritative
    /// source swap, so paths from the previous hook document are dropped.
    ReplaceMatchers { matchers: Vec<String>, cwd: PathBuf },
}

/// Cloneable configuration control for a live FileChanged watcher. The
/// runtime keeps ownership of the supervisor task; bridge-server receives only
/// this sender, so hot reload cannot accidentally detach the watcher.
#[derive(Clone, Debug)]
pub struct FileChangedWatcherController {
    control: mpsc::UnboundedSender<WatcherControl>,
}

impl FileChangedWatcherController {
    /// Replace every settings-derived matcher and restart the watched paths.
    pub fn replace_matchers(&self, matchers: Vec<String>, cwd: PathBuf) {
        let _ = self
            .control
            .send(WatcherControl::ReplaceMatchers { matchers, cwd });
    }
}

/// Cloneable owner of the spawned watcher supervisor. Hosts use
/// [`Self::shutdown_and_drain`] for an immediate join; dropping the last clone
/// retains the legacy abort-on-drop fallback.
#[derive(Debug, Clone)]
pub struct FileChangedWatcherHandle {
    owner: Arc<FileChangedWatcherOwner>,
}

#[derive(Debug)]
struct FileChangedWatcherOwner {
    /// Control channel to the supervisor (`None` for an empty handle).
    control: Option<mpsc::UnboundedSender<WatcherControl>>,
    shutdown: CancellationToken,
    state: std::sync::Mutex<FileChangedWatcherState>,
    drained: tokio::sync::Notify,
}

#[derive(Debug)]
enum FileChangedWatcherState {
    Running(Option<JoinHandle<()>>),
    Draining,
    Drained,
}

impl FileChangedWatcherOwner {
    fn begin_drain(self: &Arc<Self>) {
        let supervisor = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *state {
                FileChangedWatcherState::Running(supervisor) => {
                    let supervisor = supervisor.take();
                    *state = FileChangedWatcherState::Draining;
                    Some(supervisor)
                }
                FileChangedWatcherState::Draining | FileChangedWatcherState::Drained => None,
            }
        };
        let Some(supervisor) = supervisor else {
            return;
        };
        let Some(supervisor) = supervisor else {
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                FileChangedWatcherState::Drained;
            self.drained.notify_waiters();
            return;
        };
        // Do not abort the supervisor here: it must retain and join its child
        // loops. Cancelling its control/restart future lets that final drain
        // run even when a filesystem watch setup is currently awaiting.
        self.shutdown.cancel();
        let owner = self.clone();
        tokio::spawn(async move {
            let _ = supervisor.await;
            *owner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                FileChangedWatcherState::Drained;
            owner.drained.notify_waiters();
        });
    }

    async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if matches!(
                &*self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                FileChangedWatcherState::Drained
            ) {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for FileChangedWatcherOwner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let FileChangedWatcherState::Running(Some(supervisor)) = state {
            supervisor.abort();
        }
    }
}

impl FileChangedWatcherHandle {
    /// An empty handle that owns no tasks (e.g. when no `FileChanged` hook is
    /// configured). Dropping it is a no-op.
    #[must_use]
    pub fn empty() -> Self {
        Self::from_parts(None, None, CancellationToken::new())
    }

    fn from_parts(
        control: Option<mpsc::UnboundedSender<WatcherControl>>,
        supervisor: Option<JoinHandle<()>>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            owner: Arc::new(FileChangedWatcherOwner {
                control,
                shutdown,
                state: std::sync::Mutex::new(FileChangedWatcherState::Running(supervisor)),
                drained: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Stop the supervisor and join all directory loops. The first caller
    /// transfers the JoinHandle to an owned completion task; concurrent or
    /// cancelled waiters cannot report completion before all firers release.
    pub async fn shutdown_and_drain(&self) {
        self.owner.begin_drain();
        self.owner.wait_drained().await;
    }

    /// `updateWatchPaths`: add resolved (absolute) paths to the live watch set
    /// and restart over the union when any are new. Mirrors claude-code's
    /// returned-interface `updateWatchPaths` (`fileChangedWatcher.ts`). A no-op
    /// on an empty handle or once the supervisor has exited (the send fails and
    /// is swallowed). The paths must already be absolute — callers resolve
    /// relative matchers against the current cwd before calling.
    pub fn update_watch_paths(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if let Some(tx) = &self.owner.control {
            let _ = tx.send(WatcherControl::AddWatchPaths(paths));
        }
    }

    /// `onCwdChanged`: the persistent shell cwd moved to `new_cwd` — re-resolve
    /// the original `FileChanged` matchers against it, REPLACE the watch set, and
    /// restart. Mirrors the watcher-rebind half of claude-code's `onCwdChanged`
    /// (`fileChangedWatcher.ts` function `g`). The supervisor guards an unchanged
    /// cwd (`if(_===S)return`), so callers may fire unconditionally. A no-op on
    /// an empty handle or once the supervisor has exited (the send is swallowed).
    /// This does NOT fire the `CwdChanged` hooks — those fire via the Bash tool's
    /// `CwdChanged` firer, so the two never double-fire.
    pub fn on_cwd_changed(&self, new_cwd: PathBuf) {
        if let Some(tx) = &self.owner.control {
            let _ = tx.send(WatcherControl::CwdChanged { new_cwd });
        }
    }

    /// A [`hooks::WatcherRebinder`] over this watcher's control channel, or `None`
    /// for an empty handle (no supervisor to signal). The composition root wires
    /// this into the orchestrator's `CwdChanged` firer so a mid-session `cd`
    /// rebinds the watcher. Cloning the sender lets the rebinder outlive a move
    /// of the handle (the runtime still owns the handle for RAII teardown).
    #[must_use]
    pub fn rebinder(&self) -> Option<Arc<dyn hooks::WatcherRebinder>> {
        self.owner
            .control
            .clone()
            .map(|control| Arc::new(ControlRebinder { control }) as Arc<dyn hooks::WatcherRebinder>)
    }

    /// A cloneable hot-reload controller, when the supervisor is running.
    #[must_use]
    pub fn controller(&self) -> Option<FileChangedWatcherController> {
        self.owner
            .control
            .clone()
            .map(|control| FileChangedWatcherController { control })
    }
}

/// A [`hooks::WatcherRebinder`] backed by a clone of the supervisor's control
/// channel. Lets the `CwdChanged` firer signal a cwd rebind without owning the
/// watcher handle (which the runtime holds for RAII teardown). Fire-and-forget:
/// a send onto a closed channel (supervisor gone) is swallowed.
struct ControlRebinder {
    control: mpsc::UnboundedSender<WatcherControl>,
}

impl hooks::WatcherRebinder for ControlRebinder {
    fn rebind(&self, new_cwd: PathBuf) {
        let _ = self.control.send(WatcherControl::CwdChanged { new_cwd });
    }
}

/// A late-bound [`hooks::WatcherRebinder`]: the orchestrator's `CwdChanged` firer
/// is constructed BEFORE the file-changed watcher spawns, so it holds this
/// deferred rebinder whose inner cell is [`set`](DeferredWatcherRebinder::set)
/// once the watcher exists. Until then — and forever, when no watcher spawns at
/// all (no `FileChanged` hooks configured) — `rebind` is a silent no-op,
/// matching claude-code's `if(o)m()` (restart only runs when the watcher was
/// initialized). Cheaply cloneable: clones share the same inner cell, so the
/// firer's clone observes a `set` on the composition-root's copy.
#[derive(Clone, Default)]
pub struct DeferredWatcherRebinder {
    inner: Arc<std::sync::Mutex<Option<Arc<dyn hooks::WatcherRebinder>>>>,
}

impl DeferredWatcherRebinder {
    /// An empty deferred rebinder (its cell unset). `rebind` is a no-op until
    /// [`set`](DeferredWatcherRebinder::set) populates it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Populate the cell with the concrete rebinder once the watcher exists.
    /// Every clone shares the cell, so the firer's clone rebinds through it.
    pub fn set(&self, rebinder: Arc<dyn hooks::WatcherRebinder>) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(rebinder);
        }
    }
}

impl hooks::WatcherRebinder for DeferredWatcherRebinder {
    fn rebind(&self, new_cwd: PathBuf) {
        let inner = self.inner.lock().ok().and_then(|g| g.clone());
        if let Some(inner) = inner {
            inner.rebind(new_cwd);
        }
    }
}

/// The file-changed watcher. Owns the resolved watch paths + the fire seam and
/// spawns the supervisor that drives the per-directory watch loops.
pub struct FileChangedWatcher {
    /// The ORIGINAL `FileChanged` hook matchers (pipe-separated filename lists),
    /// retained so a mid-session cwd move can re-resolve them against the new cwd
    /// (`onCwdChanged`). Without this, only the already-resolved absolute paths
    /// survive and a relative matcher could never be re-anchored.
    matchers: Vec<String>,
    /// The set of file paths to fire for (resolved from the hook matchers).
    watch_paths: Vec<PathBuf>,
    /// The deduplicated parent directories initially watched.
    watch_dirs: Vec<PathBuf>,
    /// The cwd the matchers resolved against — the parent-directory fallback for
    /// paths added later via the dynamic `watchPaths` feedback loop, and the
    /// baseline the `onCwdChanged` guard compares a new cwd against.
    cwd: PathBuf,
    firer: Arc<dyn FileChangedFirer>,
}

impl FileChangedWatcher {
    /// Construct a watcher from the `FileChanged` hook matchers, resolved against
    /// `cwd`, firing through `firer` (the orchestrator firer in production).
    #[must_use]
    pub fn new(matchers: &[&str], cwd: &Path, firer: Arc<dyn FileChangedFirer>) -> Self {
        let watch_paths = resolve_watch_paths(matchers, cwd);
        let watch_dirs = watch_dirs_for(&watch_paths, cwd);
        Self {
            matchers: matchers.iter().map(|m| (*m).to_string()).collect(),
            watch_paths,
            watch_dirs,
            cwd: cwd.to_path_buf(),
            firer,
        }
    }

    /// The resolved watch file paths (exposed for tests / diagnostics).
    #[must_use]
    pub fn watch_paths(&self) -> &[PathBuf] {
        &self.watch_paths
    }

    /// The deduplicated parent directories watched (exposed for tests).
    #[must_use]
    pub fn watch_dirs(&self) -> &[PathBuf] {
        &self.watch_dirs
    }

    /// Spawn the supervisor (and its per-directory watch loops) via the injected
    /// `FileSystem` and return the owning [`FileChangedWatcherHandle`].
    ///
    /// Only directories that currently exist are watched (the in-tree
    /// `watch_dir_with_debounce` errors on a missing target); a directory that
    /// appears later is simply not observed until it is (re)added — matching
    /// claude-code's init-time path resolution. Best-effort: a directory that
    /// fails to watch is logged and skipped, never fatal. When the resolved
    /// When the resolved watch-path set is empty, the supervisor remains idle
    /// with no per-directory tasks. Keeping its control channel alive lets a
    /// later Desktop hook save add the first FileChanged matcher without a
    /// process restart while retaining the same no-files-watched behavior.
    pub async fn spawn(self, fs: Arc<dyn FileSystem>) -> FileChangedWatcherHandle {
        let Self {
            matchers,
            watch_paths,
            watch_dirs: _,
            cwd,
            firer,
        } = self;
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let supervisor = Supervisor {
            fs,
            firer,
            control_tx: control_tx.clone(),
            matchers,
            cwd,
            watch_paths: watch_paths.into_iter().collect(),
            dir_tasks: JoinSet::new(),
        };
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(supervisor.run(control_rx, shutdown.clone()));
        FileChangedWatcherHandle::from_parts(Some(control_tx), Some(handle), shutdown)
    }
}

/// The long-lived supervisor: owns the current watch set + the per-directory
/// loops, and restarts them on `updateWatchPaths`. Mirrors the closure state in
/// claude-code's `startFileChangedWatcher` (the watcher `e`, its dispose `y`,
/// and `updateWatchPaths` `f`).
struct Supervisor {
    fs: Arc<dyn FileSystem>,
    firer: Arc<dyn FileChangedFirer>,
    /// Cloned into each watch loop so a fired hook's `watchPaths` can drive a
    /// restart without a back-reference to the handle.
    control_tx: mpsc::UnboundedSender<WatcherControl>,
    /// The ORIGINAL matchers, re-resolved against a new cwd on `onCwdChanged`.
    matchers: Vec<String>,
    cwd: PathBuf,
    watch_paths: BTreeSet<PathBuf>,
    dir_tasks: JoinSet<()>,
}

impl Supervisor {
    async fn run(
        mut self,
        control_rx: mpsc::UnboundedReceiver<WatcherControl>,
        shutdown: CancellationToken,
    ) {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {}
            () = self.run_controls(control_rx) => {}
        }
        self.drain_directory_loops().await;
    }

    /// Arm the current watch set, then service updates until shutdown cancels
    /// this future. Child handles stay in the supervisor even during restart.
    async fn run_controls(&mut self, mut control_rx: mpsc::UnboundedReceiver<WatcherControl>) {
        self.restart().await;
        while let Some(cmd) = control_rx.recv().await {
            match cmd {
                WatcherControl::AddWatchPaths(paths) => {
                    let mut changed = false;
                    for p in paths {
                        if self.watch_paths.insert(p) {
                            changed = true;
                        }
                    }
                    // `if (v.length > 0) updateWatchPaths(v)` restarts the
                    // watcher; a no-op union (all paths already watched) leaves
                    // the live loops untouched (no needless FD churn).
                    if changed {
                        self.restart().await;
                    }
                }
                WatcherControl::CwdChanged { new_cwd } => {
                    // Watcher-rebind half of claude-code's `onCwdChanged`
                    // (function `g`). Guard an unchanged cwd (`if(_===S)return`):
                    // `self.cwd` is the cwd the current matchers resolved
                    // against.
                    if new_cwd == self.cwd {
                        continue;
                    }
                    // Re-resolve the ORIGINAL matchers against the new cwd and
                    // REPLACE the watch set — NOT a union (`r=x.watchPaths`, not
                    // the `Mo([...w,...r])` of `updateWatchPaths`). Any paths
                    // added earlier via the dynamic `watchPaths` feedback loop are
                    // intentionally dropped, matching claude-code where `g`
                    // reassigns `r` from the re-resolution. The `CwdChanged` hooks
                    // are fired shell-side (the Bash tool's `CwdChanged` firer),
                    // so this NEVER fires them — no double-fire.
                    let matcher_refs: Vec<&str> =
                        self.matchers.iter().map(String::as_str).collect();
                    self.watch_paths = resolve_watch_paths(&matcher_refs, &new_cwd)
                        .into_iter()
                        .collect();
                    self.cwd = new_cwd;
                    self.restart().await;
                }
                WatcherControl::ReplaceMatchers { matchers, cwd } => {
                    let matcher_refs: Vec<&str> = matchers.iter().map(String::as_str).collect();
                    self.watch_paths = resolve_watch_paths(&matcher_refs, &cwd)
                        .into_iter()
                        .collect();
                    self.matchers = matchers;
                    self.cwd = cwd;
                    self.restart().await;
                }
            }
        }
    }

    /// Tear down the current per-directory loops and re-arm them over the
    /// current `watch_paths` set (the parent directories, deduped). Aborting the
    /// old directory tasks first releases their `notify` handles before the
    /// new set opens — mirroring `dispose` (`y`) then re-`watch`.
    async fn restart(&mut self) {
        self.drain_directory_loops().await;
        let paths: Vec<PathBuf> = self.watch_paths.iter().cloned().collect();
        let dirs = watch_dirs_for(&paths, &self.cwd);
        let watch_set: Arc<BTreeSet<PathBuf>> = Arc::new(self.watch_paths.clone());
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            let dir_str = dir.to_string_lossy().into_owned();
            let stream = match self.fs.watch(&dir_str).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, dir = %dir_str, "file-changed watch failed");
                    continue;
                }
            };
            let watch_set = watch_set.clone();
            let firer = self.firer.clone();
            let control_tx = self.control_tx.clone();
            self.dir_tasks.spawn(async move {
                run_watch_loop(stream, watch_set, firer, control_tx).await;
            });
        }
    }

    async fn drain_directory_loops(&mut self) {
        self.dir_tasks.abort_all();
        while let Some(result) = self.dir_tasks.join_next().await {
            if let Err(error) = result {
                if !error.is_cancelled() {
                    tracing::warn!(%error, "file-changed watch task failed");
                }
            }
        }
    }
}

/// Drive one directory's change stream, firing per matching event and forwarding
/// any fired-hook `watchPaths` back to the supervisor for a restart. Split out so
/// tests can drive it with a synthetic stream (no real `FSEvents`).
async fn run_watch_loop(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
    watch_paths: Arc<BTreeSet<PathBuf>>,
    firer: Arc<dyn FileChangedFirer>,
    control_tx: mpsc::UnboundedSender<WatcherControl>,
) {
    while let Some(event) = stream.next().await {
        let added = handle_event(&event, &watch_paths, firer.as_ref()).await;
        // `if (v.length > 0) updateWatchPaths(v)` — a fired hook that returned
        // `watchPaths` restarts the watcher over the added paths.
        if !added.is_empty() {
            let _ = control_tx.send(WatcherControl::AddWatchPaths(added));
        }
    }
}

/// Fire the `FileChanged` hook for a single [`FileEvent`] when its path is one
/// of the resolved watch paths, returning any `watchPaths` the fired hooks
/// produced (empty when the path is not watched or no hook added paths). A
/// non-watched path is silently ignored (mirrors chokidar firing only for the
/// exact watched paths). Exposed for deterministic unit tests.
async fn handle_event(
    event: &FileEvent,
    watch_paths: &BTreeSet<PathBuf>,
    firer: &dyn FileChangedFirer,
) -> Vec<PathBuf> {
    if !watch_paths.contains(&event.path) {
        return Vec::new();
    }
    firer
        .fire(FileChangedFire {
            path: event.path.clone(),
            kind: kind_to_event(event.kind).to_string(),
        })
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Recording fake firer — captures every `(path, kind)` the watcher fires so
    /// tests can assert deterministically without a real orchestrator. `respond`
    /// is the set of (already-resolved) `watchPaths` each fire returns, letting a
    /// test drive the dynamic `updateWatchPaths` feedback loop.
    #[derive(Default)]
    struct RecordingFirer {
        fired: Mutex<Vec<(PathBuf, String)>>,
        respond: Vec<PathBuf>,
    }

    impl RecordingFirer {
        fn responding_with(paths: Vec<PathBuf>) -> Self {
            Self {
                fired: Mutex::new(Vec::new()),
                respond: paths,
            }
        }
    }

    #[async_trait::async_trait]
    impl FileChangedFirer for RecordingFirer {
        async fn fire(&self, fire: FileChangedFire) -> Vec<PathBuf> {
            self.fired.lock().unwrap().push((fire.path, fire.kind));
            self.respond.clone()
        }
    }

    fn ev(path: &str, kind: FileEventKind) -> FileEvent {
        FileEvent {
            path: PathBuf::from(path),
            kind,
        }
    }

    /// Minimal fake `FileSystem` for the supervisor restart test: records every
    /// directory `watch` is asked to observe and returns an immediately-ending
    /// empty event stream (the test only observes which directories are watched,
    /// not real FSEvents). Every other method is an inert stub.
    #[derive(Default)]
    struct RecordingFs {
        watched: Arc<Mutex<Vec<String>>>,
        blocked_stream: Option<Arc<StreamDropGate>>,
    }

    #[derive(Default)]
    struct StreamDropGate {
        polled: tokio::sync::Notify,
        dropping: tokio::sync::Notify,
        released: Mutex<bool>,
        release: std::sync::Condvar,
    }

    struct BlockingDropStream(Arc<StreamDropGate>);

    impl Stream for BlockingDropStream {
        type Item = FileEvent;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<FileEvent>> {
            self.0.polled.notify_one();
            std::task::Poll::Pending
        }
    }

    impl Drop for BlockingDropStream {
        fn drop(&mut self) {
            self.0.dropping.notify_one();
            let released = self.0.released.lock().unwrap();
            // Bound even the deliberately broken implementation: a failing
            // assertion must never strand a Tokio worker indefinitely.
            let _ = self
                .0
                .release
                .wait_timeout_while(released, std::time::Duration::from_secs(5), |ready| !*ready)
                .unwrap();
        }
    }

    #[async_trait::async_trait]
    impl FileSystem for RecordingFs {
        async fn read_file(
            &self,
            _p: &str,
            _o: Option<u64>,
            _l: Option<u64>,
        ) -> Result<lingxi_core::host::FileContent, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io("unused".into()))
        }
        async fn write_file(&self, _p: &str, _c: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        fn is_within_workspace(&self, _p: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            dir: &str,
        ) -> Result<
            std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
            lingxi_core::host::FsError,
        > {
            self.watched.lock().unwrap().push(dir.to_string());
            if let Some(gate) = self.blocked_stream.as_ref() {
                return Ok(Box::pin(BlockingDropStream(gate.clone())));
            }
            Ok(Box::pin(tokio_stream::iter(Vec::<FileEvent>::new())))
        }
        async fn append_file(&self, _p: &str, _c: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn truncate(&self, _p: &str, _l: u64) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn file_mtime(
            &self,
            _p: &str,
        ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, _p: &str) -> Result<u64, lingxi_core::host::FsError> {
            Ok(0)
        }
        async fn delete_file(&self, _p: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn symlink(&self, _t: &str, _l: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn flock_exclusive(
            &self,
            _p: &str,
        ) -> Result<Box<dyn lingxi_core::host::FlockGuard>, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io("unused".into()))
        }
        async fn fsync(&self, _p: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
    }

    #[test]
    fn resolve_watch_paths_splits_pipes_and_resolves_relative() {
        let cwd = Path::new("/work/proj");
        // Mirrors `.envrc|.env` plus an absolute path; whitespace is trimmed.
        let paths = resolve_watch_paths(&[".envrc | .env", "/etc/abs.conf"], cwd);
        assert!(paths.contains(&PathBuf::from("/work/proj/.envrc")));
        assert!(paths.contains(&PathBuf::from("/work/proj/.env")));
        assert!(paths.contains(&PathBuf::from("/etc/abs.conf")));
        assert_eq!(paths.len(), 3, "deduped + all three resolved");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn shutdown_waits_for_directory_children_after_a_waiter_is_cancelled() {
        let root = tempfile::tempdir().unwrap();
        let gate = Arc::new(StreamDropGate::default());
        let fs = Arc::new(RecordingFs {
            blocked_stream: Some(gate.clone()),
            ..Default::default()
        });
        let firer = Arc::new(RecordingFirer::default());
        let weak_firer = Arc::downgrade(&firer);
        let handle = FileChangedWatcher::new(&[".env"], root.path(), firer)
            .spawn(fs)
            .await;
        gate.polled.notified().await;
        let first_handle = handle.clone();
        let first = tokio::spawn(async move { first_handle.shutdown_and_drain().await });
        gate.dropping.notified().await;
        first.abort();
        let _ = first.await;

        let second_handle = handle.clone();
        let mut second = tokio::spawn(async move { second_handle.shutdown_and_drain().await });
        let returned_while_child_owned =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut second)
                .await
                .is_ok();
        *gate.released.lock().unwrap() = true;
        gate.release.notify_all();
        if !returned_while_child_owned {
            tokio::time::timeout(std::time::Duration::from_secs(2), second)
                .await
                .expect("directory loop teardown completes")
                .unwrap();
        }
        assert!(
            !returned_while_child_owned,
            "joining only the supervisor must not report its child loops drained"
        );
        assert!(weak_firer.upgrade().is_none());
        handle.shutdown_and_drain().await;
    }

    #[test]
    fn resolve_watch_paths_dedups_and_skips_empty() {
        let cwd = Path::new("/work");
        // Empty segments (leading/trailing pipe) are skipped; duplicates dedup.
        let paths = resolve_watch_paths(&["|.env|", ".env"], cwd);
        assert_eq!(paths, vec![PathBuf::from("/work/.env")]);
    }

    #[test]
    fn no_matchers_yields_no_watch_paths() {
        // A matcher-less config (or only empty matchers) resolves to nothing —
        // the no-watch case (`if (paths.length === 0) return`).
        assert!(resolve_watch_paths(&[], Path::new("/work")).is_empty());
        assert!(resolve_watch_paths(&["", "  "], Path::new("/work")).is_empty());
    }

    #[test]
    fn watch_dirs_are_parents_deduped() {
        let cwd = Path::new("/work");
        let paths = vec![
            PathBuf::from("/work/.env"),
            PathBuf::from("/work/.envrc"),
            PathBuf::from("/work/sub/cfg.toml"),
        ];
        let dirs = watch_dirs_for(&paths, cwd);
        // `/work` (shared by .env + .envrc) appears once; `/work/sub` once.
        assert!(dirs.contains(&PathBuf::from("/work")));
        assert!(dirs.contains(&PathBuf::from("/work/sub")));
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn kind_maps_to_chokidar_event_names() {
        assert_eq!(kind_to_event(FileEventKind::Created), "add");
        assert_eq!(kind_to_event(FileEventKind::Modified), "change");
        assert_eq!(kind_to_event(FileEventKind::Deleted), "unlink");
    }

    #[tokio::test]
    async fn handle_event_fires_only_for_watched_paths_with_mapped_kind() {
        let watch: BTreeSet<PathBuf> = [PathBuf::from("/work/.env")].into_iter().collect();
        let firer = RecordingFirer::default();

        // Watched path, Modified → fires "change".
        handle_event(&ev("/work/.env", FileEventKind::Modified), &watch, &firer).await;
        // Watched path, Created → fires "add".
        handle_event(&ev("/work/.env", FileEventKind::Created), &watch, &firer).await;
        // Watched path, Deleted → fires "unlink".
        handle_event(&ev("/work/.env", FileEventKind::Deleted), &watch, &firer).await;
        // Sibling un-watched path in the same dir → no fire.
        handle_event(
            &ev("/work/other.txt", FileEventKind::Modified),
            &watch,
            &firer,
        )
        .await;

        let recorded = firer.fired.lock().unwrap();
        assert_eq!(recorded.len(), 3, "only the three watched-path events fire");
        assert_eq!(recorded[0], (PathBuf::from("/work/.env"), "change".into()));
        assert_eq!(recorded[1], (PathBuf::from("/work/.env"), "add".into()));
        assert_eq!(recorded[2], (PathBuf::from("/work/.env"), "unlink".into()));
    }

    #[tokio::test]
    async fn run_watch_loop_drives_synthetic_stream() {
        // Inject a synthetic change stream (no real FSEvents) and assert the loop
        // fires each watched event then exits cleanly when the stream ends —
        // deterministic, no FS timing dependency.
        let watch: Arc<BTreeSet<PathBuf>> =
            Arc::new([PathBuf::from("/work/.envrc")].into_iter().collect());
        let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::default());
        let events = vec![
            ev("/work/.envrc", FileEventKind::Modified),
            ev("/work/unrelated.rs", FileEventKind::Modified),
            ev("/work/.envrc", FileEventKind::Deleted),
        ];
        let stream = tokio_stream::iter(events);
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        run_watch_loop(Box::pin(stream), watch, firer.clone(), control_tx).await;

        let recorded = firer.fired.lock().unwrap();
        assert_eq!(recorded.len(), 2, "only the two .envrc events fire");
        assert_eq!(recorded[0].1, "change");
        assert_eq!(recorded[1].1, "unlink");
        // A firer that returns no watchPaths never enqueues a restart.
        assert!(
            control_rx.try_recv().is_err(),
            "no AddWatchPaths when the firer returns no watchPaths"
        );
    }

    #[tokio::test]
    async fn handle_event_returns_fired_hook_watch_paths() {
        // A fired hook that returns `watchPaths` surfaces them from handle_event
        // (the dynamic feedback loop's source). A non-watched path returns empty.
        let watch: BTreeSet<PathBuf> = [PathBuf::from("/work/.env")].into_iter().collect();
        let added = vec![PathBuf::from("/work/extra.cfg")];
        let firer = RecordingFirer::responding_with(added.clone());

        let got = handle_event(&ev("/work/.env", FileEventKind::Modified), &watch, &firer).await;
        assert_eq!(
            got, added,
            "fired-hook watchPaths flow through handle_event"
        );

        let none = handle_event(
            &ev("/work/unwatched.txt", FileEventKind::Modified),
            &watch,
            &firer,
        )
        .await;
        assert!(
            none.is_empty(),
            "non-watched path fires nothing, adds nothing"
        );
    }

    #[tokio::test]
    async fn run_watch_loop_forwards_fired_watch_paths_to_supervisor() {
        // The dynamic `updateWatchPaths` feedback loop: when a fired hook returns
        // watchPaths, the loop enqueues an AddWatchPaths control message.
        let watch: Arc<BTreeSet<PathBuf>> =
            Arc::new([PathBuf::from("/work/.env")].into_iter().collect());
        let added = vec![PathBuf::from("/work/added.cfg")];
        let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::responding_with(added.clone()));
        let stream = tokio_stream::iter(vec![ev("/work/.env", FileEventKind::Modified)]);
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        run_watch_loop(Box::pin(stream), watch, firer, control_tx).await;

        match control_rx.try_recv() {
            Ok(WatcherControl::AddWatchPaths(paths)) => assert_eq!(paths, added),
            other => panic!("expected AddWatchPaths({added:?}), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_watch_paths_restarts_over_the_new_directory() {
        // End-to-end supervisor restart: spawn over dir A, then push a path in a
        // new dir B via update_watch_paths and assert the fake FS is asked to
        // watch B (the union restart). Real temp dirs so `is_dir()` passes.
        let root = tempfile::tempdir().unwrap();
        let dir_a = root.path().join("a");
        let dir_b = root.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let fs = Arc::new(RecordingFs::default());
        let watched = fs.watched.clone();
        let firer: Arc<dyn FileChangedFirer> = Arc::new(RecordingFirer::default());

        // Watch a file in dir A initially.
        let a_env = dir_a.join(".env");
        let watcher = FileChangedWatcher::new(&[a_env.to_str().unwrap()], root.path(), firer);
        let handle = watcher.spawn(fs as Arc<dyn FileSystem>).await;

        // Wait for the initial watch of dir A.
        let wait_for = |needle: PathBuf| {
            let watched = watched.clone();
            async move {
                for _ in 0..400 {
                    if watched
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|d| Path::new(d) == needle)
                    {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                false
            }
        };
        assert!(wait_for(dir_a.clone()).await, "dir A watched on spawn");

        // Add a path in dir B → supervisor unions + restarts → dir B watched.
        handle.update_watch_paths(vec![dir_b.join("extra.cfg")]);
        assert!(
            wait_for(dir_b.clone()).await,
            "dir B watched after update_watch_paths restart"
        );

        drop(handle);
    }

    #[tokio::test]
    async fn replace_matchers_can_activate_an_initially_idle_watcher() {
        let root = tempfile::tempdir().unwrap();
        let watched_dir = root.path().join("configured-later");
        std::fs::create_dir_all(&watched_dir).unwrap();
        let fs = Arc::new(RecordingFs::default());
        let watched = fs.watched.clone();
        let firer: Arc<dyn FileChangedFirer> = Arc::new(RecordingFirer::default());
        let handle = FileChangedWatcher::new(&[], root.path(), firer)
            .spawn(fs as Arc<dyn FileSystem>)
            .await;
        assert!(watched.lock().unwrap().is_empty());

        handle
            .controller()
            .expect("idle watcher keeps a live controller")
            .replace_matchers(
                vec![watched_dir.join(".env").to_string_lossy().into_owned()],
                root.path().to_path_buf(),
            );

        for _ in 0..400 {
            if watched
                .lock()
                .unwrap()
                .iter()
                .any(|path| Path::new(path) == watched_dir)
            {
                drop(handle);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("replacement matcher did not start its directory watcher");
    }

    /// Spawn a watcher over a relative matcher resolved against `cwd`, waiting
    /// for the initial dir watch. Returns `(handle, watched-record, wait_for)`.
    /// Shared setup for the `on_cwd_changed` tests below.
    async fn spawn_relative(
        matcher: &str,
        cwd: &Path,
        firer: Arc<dyn FileChangedFirer>,
    ) -> (
        FileChangedWatcherHandle,
        Arc<Mutex<Vec<String>>>,
        impl Fn(PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
    ) {
        let fs = Arc::new(RecordingFs::default());
        let watched = fs.watched.clone();
        let watcher = FileChangedWatcher::new(&[matcher], cwd, firer);
        let handle = watcher.spawn(fs as Arc<dyn FileSystem>).await;
        let watched_for_wait = watched.clone();
        let wait_for = move |needle: PathBuf| {
            let watched = watched_for_wait.clone();
            Box::pin(async move {
                for _ in 0..400 {
                    if watched
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|d| Path::new(d) == needle)
                    {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                false
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        (handle, watched, wait_for)
    }

    #[tokio::test]
    async fn on_cwd_changed_rebinds_relative_matcher_to_new_cwd() {
        // The onCwdChanged watcher rebind: a relative matcher (".envrc") resolved
        // against cwd A must, after a mid-session `cd` to B, be re-resolved
        // against B — REPLACE semantics: B's dir is watched and A's dir is NOT
        // re-watched by the rebind restart (it is dropped from the new set).
        let root = tempfile::tempdir().unwrap();
        let dir_a = root.path().join("a");
        let dir_b = root.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let firer: Arc<dyn FileChangedFirer> = Arc::new(RecordingFirer::default());
        let (handle, watched, wait_for) = spawn_relative(".envrc", &dir_a, firer).await;
        assert!(wait_for(dir_a.clone()).await, "dir A watched on spawn");

        // Observe ONLY what the rebind restart watches (drop the spawn record).
        watched.lock().unwrap().clear();
        handle.on_cwd_changed(dir_b.clone());
        assert!(
            wait_for(dir_b.clone()).await,
            "dir B watched after cwd rebind (matcher re-resolved against B)"
        );
        // REPLACE not union: the rebind restart watches ONLY B, never re-watches A.
        {
            let w = watched.lock().unwrap();
            assert!(
                w.iter().any(|d| Path::new(d) == dir_b),
                "dir B in the rebind watch set"
            );
            assert!(
                !w.iter().any(|d| Path::new(d) == dir_a),
                "dir A dropped from the watch set (REPLACE, not union)"
            );
        }
        drop(handle);
    }

    #[tokio::test]
    async fn on_cwd_changed_noops_when_cwd_unchanged() {
        // Guard `if(_===S)return`: rebinding to the SAME cwd triggers no restart,
        // so the fake FS is never asked to (re-)watch anything.
        let root = tempfile::tempdir().unwrap();
        let dir_a = root.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();

        let firer: Arc<dyn FileChangedFirer> = Arc::new(RecordingFirer::default());
        let (handle, watched, wait_for) = spawn_relative(".envrc", &dir_a, firer).await;
        assert!(wait_for(dir_a.clone()).await, "dir A watched on spawn");

        watched.lock().unwrap().clear();
        handle.on_cwd_changed(dir_a.clone());
        // Give the supervisor ample time to (not) process a restart.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            watched.lock().unwrap().is_empty(),
            "unchanged cwd must not restart the watcher"
        );
        drop(handle);
    }

    #[tokio::test]
    async fn rebinder_forwards_cwd_change_to_supervisor() {
        // `handle.rebinder()` yields a WatcherRebinder whose `rebind(new_cwd)`
        // drives the same re-resolve+restart as `on_cwd_changed` — the seam the
        // composition root injects into the orchestrator's CwdChanged firer.
        // (`rebind` resolves through the `dyn WatcherRebinder` vtable — no `use`.)
        let root = tempfile::tempdir().unwrap();
        let dir_a = root.path().join("a");
        let dir_b = root.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let firer: Arc<dyn FileChangedFirer> = Arc::new(RecordingFirer::default());
        let (handle, watched, wait_for) = spawn_relative(".envrc", &dir_a, firer).await;
        assert!(wait_for(dir_a.clone()).await, "dir A watched on spawn");

        let rebinder = handle.rebinder().expect("live handle yields a rebinder");
        watched.lock().unwrap().clear();
        rebinder.rebind(dir_b.clone());
        assert!(
            wait_for(dir_b.clone()).await,
            "dir B watched after rebinder.rebind (same path as on_cwd_changed)"
        );
        drop(handle);
    }

    #[test]
    fn on_cwd_changed_and_rebinder_are_noops_on_empty_handle() {
        // An empty handle owns no supervisor: on_cwd_changed must not panic and
        // rebinder() yields None (nothing to signal). The no-FileChanged-hook
        // case — the rebind is inherently a no-op (matches claude-code's `if(o)`).
        let handle = FileChangedWatcherHandle::empty();
        handle.on_cwd_changed(PathBuf::from("/nowhere"));
        assert!(
            handle.rebinder().is_none(),
            "empty handle yields no rebinder"
        );
    }

    #[test]
    fn deferred_rebinder_forwards_only_after_set() {
        // The late-binding adapter: before `set`, rebind is a silent no-op; after
        // `set`, it forwards to the wrapped rebinder. Clones share the cell, so a
        // firer holding one clone rebinds through a `set` on another.
        use hooks::WatcherRebinder as _;
        #[derive(Default)]
        struct Recording {
            seen: Mutex<Vec<PathBuf>>,
        }
        impl hooks::WatcherRebinder for Recording {
            fn rebind(&self, new_cwd: PathBuf) {
                self.seen.lock().unwrap().push(new_cwd);
            }
        }

        let deferred = DeferredWatcherRebinder::new();
        let firer_clone = deferred.clone();
        // Before set: no-op (nothing panics, nothing recorded).
        firer_clone.rebind(PathBuf::from("/early"));

        let inner = Arc::new(Recording::default());
        deferred.set(inner.clone());
        // After set on `deferred`, the firer's clone forwards through the shared cell.
        firer_clone.rebind(PathBuf::from("/late"));
        assert_eq!(
            *inner.seen.lock().unwrap(),
            vec![PathBuf::from("/late")],
            "only the post-set rebind reaches the wrapped rebinder"
        );
    }

    #[tokio::test]
    async fn new_resolves_paths_and_dirs() {
        let firer: Arc<dyn FileChangedFirer> = Arc::new(RecordingFirer::default());
        let w = FileChangedWatcher::new(&[".env|.envrc"], Path::new("/work"), firer);
        assert_eq!(
            w.watch_paths(),
            &[PathBuf::from("/work/.env"), PathBuf::from("/work/.envrc")]
        );
        assert_eq!(w.watch_dirs(), &[PathBuf::from("/work")]);
    }
}
