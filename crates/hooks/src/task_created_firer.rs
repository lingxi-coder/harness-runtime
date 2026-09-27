//! Narrow hook-firer seam for the `TaskCreated` lifecycle event.
//!
//! Counterpart to [`crate::task_completed_firer::TaskCompletedFirer`]. The
//! `tasks` crate is a LEAF with no access to a live hook executor: it owns the
//! task lifecycle ([`TaskRegistry::create`](../../tasks/src/registry.rs)) but
//! cannot reach `orch.hooks` without a dependency cycle. This trait closes that
//! seam the same way [`TaskCompletedFirer`](crate::task_completed_firer) does:
//! the registry holds an `Option<Arc<dyn TaskCreatedFirer>>` (default `None` =>
//! strict no-op) and calls [`fire`](TaskCreatedFirer::fire) best-effort when a
//! task is created.
//!
//! Defining the trait HERE (the `hooks` crate) — rather than in `tasks` — lets
//! the ORCHESTRATOR implement it over its `Arc<HookExecutorImpl>` (the
//! orchestrator depends on `hooks` but NOT on `tasks`, so it could never name a
//! `tasks`-defined trait). The composition root (`engine-desktop`) constructs
//! the impl and injects it where the `TaskRegistry` is built. This mirrors
//! [`OrchestratorTaskCompletedFirer`] and `OrchestratorHookDispatcher` (the MCP
//! elicitation firer).
//!
//! Parity: claude-code fires `executeTaskCreatedHooks` (`utils/hooks.ts:3745`)
//! when a task is created — from `TaskCreateTool` (`TaskCreateTool.ts:93`). The
//! fire is best-effort here: a blocking/failing hook is surfaced upstream in
//! claude-code, but the Rust seam degrades to "no intervention" so a
//! misbehaving hook never breaks task creation. (claude-code's `TaskCreate`
//! tool deletes the task and throws if a hook blocks; the Rust registry has no such
//! rollback path, so this seam is observe-only — the same divergence the
//! `TaskCompletedFirer` carries.)
//!
//! [`OrchestratorTaskCompletedFirer`]: ../../orchestrator/src/task_completed_firer.rs

use std::sync::Arc;

use async_trait::async_trait;

/// The byte-faithful wire payload for a `TaskCreated` fire, sourced from the
/// task at creation. Field set + optionality mirror
/// `TaskCreatedHookInputSchema` (`coreSchemas.ts:601-612`) — identical to the
/// `TaskCompleted` schema minus the `status` routing tag (creation has no
/// terminal status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCreatedFire {
    /// Caller-supplied or engine-generated task ID (wire `task_id`).
    pub task_id: String,
    /// Task taxonomy bucket / subject (wire `task_subject`, required).
    pub task_subject: String,
    /// Task description (wire `task_description`, optional).
    pub task_description: Option<String>,
    /// Name of the teammate creating the task (wire `teammate_name`, optional).
    pub teammate_name: Option<String>,
    /// Team the teammate belongs to (wire `team_name`, optional).
    pub team_name: Option<String>,
}

/// One-method seam the `tasks` crate uses to fire a `TaskCreated` hook without
/// owning a hook executor. The default registry holds `None` and performs a
/// strict no-op; the orchestrator provides the real impl.
#[async_trait]
pub trait TaskCreatedFirer: Send + Sync {
    /// Fire the `TaskCreated` hook for a newly-created task.
    ///
    /// Best-effort: implementations MUST NOT propagate hook failures — a
    /// failing or absent hook is swallowed so the caller's task creation always
    /// completes.
    async fn fire(&self, fire: TaskCreatedFire);
}

/// Convenience alias for the optional firer a [`TaskRegistry`] holds.
///
/// [`TaskRegistry`]: ../../tasks/src/registry.rs
pub type OptionalTaskCreatedFirer = Option<Arc<dyn TaskCreatedFirer>>;
