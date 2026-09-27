//! Narrow hook-firer seam for the `TaskCompleted` lifecycle event.
//!
//! The `tasks` crate is a LEAF with no access to a live hook executor: it owns
//! the task lifecycle ([`TaskRegistry::set_status`](../../tasks/src/registry.rs))
//! but cannot reach `orch.hooks` without a dependency cycle. This trait closes
//! that seam the same way `platform_api::team_spawn::TeamSpawnSeam` does for the
//! coordinator → tasks edge: the registry holds an
//! `Option<Arc<dyn TaskCompletedFirer>>` (default `None` => strict no-op) and
//! calls [`fire`](TaskCompletedFirer::fire) best-effort when a task transitions
//! to a TERMINAL status.
//!
//! Defining the trait HERE (the `hooks` crate) — rather than in `tasks` — lets
//! the ORCHESTRATOR implement it over its `Arc<HookExecutorImpl>` (the
//! orchestrator depends on `hooks` but NOT on `tasks`, so it could never name a
//! `tasks`-defined trait). The composition root (`engine-desktop`) constructs
//! the impl and injects it where the `TaskRegistry` is built. This mirrors
//! `OrchestratorHookDispatcher` (the MCP elicitation firer).
//!
//! Parity: claude-code fires `executeTaskCompletedHooks` (`utils/hooks.ts`)
//! when a task is marked terminal — from `TaskUpdateTool` (status →
//! `completed`) and from `stopHooks.ts` (a teammate's in-progress tasks on
//! stop). The fire is best-effort: a blocking/failing hook is surfaced upstream
//! in claude-code, but the Rust seam degrades to "no intervention" so a
//! misbehaving hook never breaks the status transition.

use std::sync::Arc;

use async_trait::async_trait;

/// The byte-faithful wire payload for a `TaskCompleted` fire, sourced from the
/// task at the terminal transition. Field set + optionality mirror
/// `TaskCompletedHookInputSchema` (`coreSchemas.ts:614-625`); `status` is
/// carried for routing only (it is NOT part of the wire payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCompletedFire {
    /// Task ID matching the prior `TaskCreated` (wire `task_id`).
    pub task_id: String,
    /// Terminal status string (`"completed"` / `"failed"`). Routing only.
    pub status: String,
    /// Task subject/title (wire `task_subject`, required).
    pub task_subject: String,
    /// Task description (wire `task_description`, optional).
    pub task_description: Option<String>,
    /// Name of the teammate completing the task (wire `teammate_name`,
    /// optional).
    pub teammate_name: Option<String>,
    /// Team the teammate belongs to (wire `team_name`, optional).
    pub team_name: Option<String>,
}

/// One-method seam the `tasks` crate uses to fire a `TaskCompleted` hook
/// without owning a hook executor. The default registry holds `None` and
/// performs a strict no-op; the orchestrator provides the real impl.
#[async_trait]
pub trait TaskCompletedFirer: Send + Sync {
    /// Fire the `TaskCompleted` hook for a terminal task transition.
    ///
    /// Best-effort: implementations MUST NOT propagate hook failures — a
    /// failing or absent hook is swallowed so the caller's status transition
    /// always completes.
    async fn fire(&self, fire: TaskCompletedFire);
}

/// Convenience alias for the optional firer a [`TaskRegistry`] holds.
///
/// [`TaskRegistry`]: ../../tasks/src/registry.rs
pub type OptionalTaskCompletedFirer = Option<Arc<dyn TaskCompletedFirer>>;
