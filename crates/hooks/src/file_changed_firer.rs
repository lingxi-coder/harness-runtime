//! Narrow hook-firer seam for the `FileChanged` lifecycle event.
//!
//! Sibling of [`crate::cwd_changed_firer::CwdChangedFirer`] — both events
//! originate in claude-code's `utils/hooks/fileChangedWatcher.ts`. Where the
//! `CwdChanged` firer is driven by a `pwd -P` readback inside the shell tool,
//! the `FileChanged` firer is driven by a long-lived filesystem watcher: the
//! composition root resolves the watch paths from the user's `FileChanged` hook
//! config, watches them via [`platform_api::FileSystem::watch`], and fires this seam
//! once per debounced change (`fileChangedWatcher.ts:80-106`).
//!
//! The trait is defined HERE (the `hooks` crate) — not in the watcher's home
//! crate (`engine-desktop`) — so the ORCHESTRATOR can implement it over its
//! `Arc<HookExecutorImpl>` (the orchestrator depends on `hooks`). The
//! composition root constructs an `OrchestratorFileChangedFirer` over the SAME
//! `Arc<HookExecutorImpl>` it hands the orchestrator and injects it into the
//! desktop file-changed watcher. This mirrors the
//! [`CwdChangedFirer`](crate::cwd_changed_firer::CwdChangedFirer) /
//! `OrchestratorTaskCreatedFirer` seams.
//!
//! Parity: claude-code's `handleFileEvent` (`fileChangedWatcher.ts:80`) calls
//! `executeFileChangedHooks(path, event)` (`utils/hooks.ts:4278-4294`) with
//! `event` one of `'change' | 'add' | 'unlink'` — the chokidar event names.
//! The wire payload (`FileChangedHookInput`, `coreSchemas.ts:737-745`) carries
//! `file_path` + `event`. The fire is best-effort: a failing/absent hook
//! degrades to a no-op and never breaks the watch loop — matching the
//! `CwdChanged` / task firers.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

/// The byte-faithful wire payload for a `FileChanged` fire, sourced from the
/// desktop file-changed watcher. Field set mirrors `executeFileChangedHooks`'s
/// `(filePath, event)` arguments (`utils/hooks.ts:4278-4292`): `path` becomes
/// the wire `file_path`, and `kind` becomes the wire `event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangedFire {
    /// Path of the file that changed (wire `file_path`).
    pub path: PathBuf,
    /// The chokidar-style change kind — `"change"` / `"add"` / `"unlink"`
    /// (wire `event`).
    pub kind: String,
}

/// One-method seam the desktop file-changed watcher uses to fire a
/// `FileChanged` hook without owning a hook executor. The orchestrator provides
/// the real impl over its shared `Arc<HookExecutorImpl>`.
#[async_trait]
pub trait FileChangedFirer: Send + Sync {
    /// Fire the `FileChanged` hook for a watched file that mutated on disk and
    /// return any `hookSpecificOutput.watchPaths` the fired hooks produced,
    /// resolved to absolute paths (empty when none).
    ///
    /// Parity: claude-code's `handleFileEvent` runs `v3r(path, event)` and, when
    /// the returned `watchPaths` is non-empty, calls `updateWatchPaths` to
    /// restart the watcher over the added paths (`fileChangedWatcher.ts:108-131`
    /// — the "dynamic watch paths" feedback loop). The desktop watcher folds the
    /// returned paths into its watch set and restarts.
    ///
    /// Best-effort: implementations MUST NOT propagate hook failures — a
    /// failing or absent hook is swallowed (returning no paths) so the watch
    /// loop always continues.
    async fn fire(&self, fire: FileChangedFire) -> Vec<PathBuf>;
}

/// Convenience alias for an optional firer.
pub type OptionalFileChangedFirer = Option<Arc<dyn FileChangedFirer>>;
