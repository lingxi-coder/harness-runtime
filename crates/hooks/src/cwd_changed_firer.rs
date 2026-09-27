//! Narrow hook-firer seam for the `CwdChanged` lifecycle event.
//!
//! Counterpart to [`crate::task_created_firer::TaskCreatedFirer`] /
//! [`crate::task_completed_firer::TaskCompletedFirer`]. The `tool-shell` crate
//! is a LEAF tool crate: [`BashTool`](../../tools/shell/src/bash.rs) owns the
//! persistent shell cwd (`shell_cwd`, updated by a `pwd -P` readback after a
//! foreground command) but has no access to a live hook executor, and wiring
//! one through the tool-construction `BuiltinToolContext` is impossible (that
//! struct is built by a full literal in the FORBIDDEN `engine-mobile` code, so
//! a new field would break it). This trait closes the seam the same way the
//! task firers do: `BashTool` holds an `Option<Arc<dyn CwdChangedFirer>>`
//! (default `None` => strict no-op) and calls [`fire`](CwdChangedFirer::fire)
//! best-effort when the cwd actually changes.
//!
//! Defining the trait HERE (the `hooks` crate) — rather than in `tool-shell` —
//! lets the ORCHESTRATOR implement it over its `Arc<HookExecutorImpl>` (the
//! orchestrator depends on `hooks` but NOT on `tool-shell`, so it could never
//! name a `tool-shell`-defined trait). The composition root (`engine-desktop`)
//! constructs the impl and injects it via `BashTool::with_cwd_changed_firer`
//! where the desktop tool set is registered. `engine-mobile` never registers
//! the shell tools at all (`tool_shell::register_all` is desktop-only), so the
//! mobile path keeps the plain `BashTool::new(ctx)` with the firer `None`.
//! This mirrors [`OrchestratorTaskCreatedFirer`] and `OrchestratorHookDispatcher`
//! (the MCP elicitation firer).
//!
//! `tool-shell` already pulls in the `hooks`-graph crates transitively (via
//! `tool-api` → `engine`), so naming this trait adds an INTERNAL path-dep edge
//! only — no new external Cargo.lock package.
//!
//! Parity: claude-code fires `onCwdChangedForHooks(oldCwd, newCwd)`
//! (`utils/Shell.ts:409`, the `pwd -P` readback site) which dispatches
//! `executeCwdChangedHooks` (`utils/hooks.ts:4260`). The fire condition is
//! `oldCwd !== newCwd` (`fileChangedWatcher.ts:137`); the wire payload carries
//! `old_cwd` / `new_cwd` (`CwdChangedHookInput`, `types/hooks.ts:146`). The fire
//! is best-effort here: a failing/absent hook degrades to a no-op and never
//! breaks the cwd update — matching the task firers.
//!
//! [`OrchestratorTaskCreatedFirer`]: ../../orchestrator/src/task_created_firer.rs
//! [`BashTool`]: ../../tools/shell/src/bash.rs

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

/// The byte-faithful wire payload for a `CwdChanged` fire, sourced from the
/// `BashTool` cwd readback. Field set mirrors `CwdChangedHookInput`
/// (`types/hooks.ts:146`, `utils/hooks.ts:4269-4274`): `old_cwd` / `new_cwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CwdChangedFire {
    /// Previous working directory (wire `old_cwd`).
    pub old: PathBuf,
    /// New working directory after the `cd` (wire `new_cwd`).
    pub new: PathBuf,
}

/// One-method seam the `tool-shell` crate uses to fire a `CwdChanged` hook
/// without owning a hook executor. The default `BashTool` holds `None` and
/// performs a strict no-op; the orchestrator provides the real impl.
#[async_trait]
pub trait CwdChangedFirer: Send + Sync {
    /// Fire the `CwdChanged` hook for a working-directory change.
    ///
    /// Best-effort: implementations MUST NOT propagate hook failures — a
    /// failing or absent hook is swallowed so the caller's cwd update always
    /// completes. The caller only fires when `fire.old != fire.new`
    /// (claude-code's `oldCwd !== newCwd` guard).
    async fn fire(&self, fire: CwdChangedFire);
}

/// Convenience alias for the optional firer a [`BashTool`](../../tools/shell/src/bash.rs)
/// holds.
pub type OptionalCwdChangedFirer = Option<Arc<dyn CwdChangedFirer>>;
