//! Narrow seam for rebinding the desktop file-changed watcher when the
//! persistent shell cwd moves mid-session.
//!
//! Sibling of [`crate::cwd_changed_firer::CwdChangedFirer`]. Both close the same
//! kind of dependency-inversion gap: the leaf that owns the signal (the shell
//! tool's persistent-cwd tracking) cannot reach the composition-root object that
//! must react (the file-changed watcher), so the trait lives HERE (the `hooks`
//! crate) where the ORCHESTRATOR can name it. The composition root
//! (`engine-desktop`) provides the concrete impl over the spawned watcher's
//! control channel and hands it to the orchestrator's `CwdChanged` firer.
//!
//! Parity: the watcher-rebind half of claude-code's `onCwdChanged`
//! (`fileChangedWatcher.ts`, function `g`). After a `cd` moves the persistent
//! shell cwd, claude-code (a) fires the `CwdChanged` hooks and (b) re-resolves
//! the `FileChanged` matchers against the NEW cwd (REPLACING the watch set) and
//! restarts the watcher. Half (a) is already reproduced in the Rust port by the
//! Bash tool firing [`CwdChangedFirer`](crate::cwd_changed_firer::CwdChangedFirer)
//! on the same `pwd -P` readback; this trait carries half (b) — the pure
//! watcher rebind — so the two never double-fire the `CwdChanged` hooks.
//!
//! Fire-and-forget: `rebind` only signals the watcher (an unbounded channel
//! send), so it MUST NOT block or propagate errors. A rebind against an
//! already-torn-down watcher, or when no watcher exists at all (no `FileChanged`
//! hooks configured), is a silent no-op — matching claude-code, where the
//! watcher's restart step only runs when it was initialized (`if(o)m()`).

use std::path::PathBuf;
use std::sync::Arc;

/// One-method seam the `CwdChanged` firer uses to ask the desktop file-changed
/// watcher to re-resolve its `FileChanged` matchers against a new cwd and
/// restart, without owning the watcher itself. The composition root supplies the
/// real impl over the spawned watcher's control channel.
pub trait WatcherRebinder: Send + Sync {
    /// Signal the file-changed watcher that the persistent shell cwd moved to
    /// `new_cwd`.
    ///
    /// The watcher re-resolves its original `FileChanged` matchers against
    /// `new_cwd`, REPLACES its watch set (claude-code's `r=x.watchPaths`, not a
    /// union), and restarts. Guarding an unchanged cwd (claude-code's
    /// `if(_===S)return`) is the watcher's responsibility, so callers may fire
    /// unconditionally. MUST NOT block or error — a rebind on a torn-down or
    /// never-spawned watcher is a silent no-op.
    fn rebind(&self, new_cwd: PathBuf);
}

/// Convenience alias for the optional rebinder the orchestrator's `CwdChanged`
/// firer holds. `None` (mobile, or a desktop session with no `FileChanged`
/// hooks) makes the rebind a strict no-op.
pub type OptionalWatcherRebinder = Option<Arc<dyn WatcherRebinder>>;
