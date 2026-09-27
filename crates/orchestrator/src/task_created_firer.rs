//! Orchestrator-side [`hooks::TaskCreatedFirer`] implementation.
//!
//! Counterpart to [`OrchestratorTaskCompletedFirer`](crate::OrchestratorTaskCompletedFirer).
//! The `tasks` crate is a LEAF: it owns the task lifecycle
//! ([`tasks::registry::TaskRegistry::create`]) but has no access to a live hook
//! executor, and the orchestrator depends on `hooks` but NOT on `tasks`. The
//! firer trait therefore lives in `hooks` (both crates can name it); this
//! adapter closes the seam from the orchestrator side: it owns the engine's
//! `Arc<hooks::HookExecutorImpl>` (`orch.hooks`) plus the engine cwd, translates
//! a [`hooks::TaskCreatedFire`] into a [`hooks::HookEvent::TaskCreated`], and
//! fires the registry best-effort.
//!
//! Parity: reproduces claude-code's `executeTaskCreatedHooks`
//! (`utils/hooks.ts:3745`, fired from `TaskCreateTool.ts:93`) — the wire payload
//! (`TaskCreatedHookInputSchema`, `coreSchemas.ts:601-612`) carries `task_id` /
//! `task_subject` / `task_description` / `teammate_name` / `team_name`.
//!
//! Best-effort: [`HookExecutorImpl::execute`] never errors out, so a failing or
//! absent `TaskCreated` hook degrades to a no-op and never breaks the task's
//! creation — matching the `TaskCompleted` / `subagent_stop` arms.
//! (claude-code's `TaskCreate` tool rolls the task back and throws when a hook
//! blocks; the Rust registry has no such rollback path, so this seam is
//! observe-only.)
//!
//! Wiring: the composition root (`engine-desktop`) builds an
//! [`OrchestratorTaskCreatedFirer`] over the SAME `Arc<HookExecutorImpl>` it
//! hands the orchestrator, then injects it via
//! `TaskRegistry::with_task_created_firer` (an `Option`, default `None` => no
//! fire).
//!
//! [`tasks::registry::TaskRegistry::create`]: ../../tasks/src/registry.rs

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use hooks::task_created_firer::{TaskCreatedFire, TaskCreatedFirer};
use hooks::HookExecutorImpl;

/// Adapts the engine's hook executor to the `tasks` crate's `TaskCreated` firer
/// seam.
pub struct OrchestratorTaskCreatedFirer {
    /// The SAME executor the orchestrator fires its other hooks through.
    hooks: Arc<HookExecutorImpl>,
    /// Engine cwd, threaded into the `TaskCreated` hook payload (`cwd`) and the
    /// per-hook Command-arm `LINGXI_PROJECT_DIR` fallback.
    cwd: PathBuf,
    /// The MAIN orchestrator session's transcript path
    /// (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
    /// `getTranscriptPathForSession`), stamped on the `TaskCreated` hook payload's
    /// `transcript_path`. Computed once at the composition root from the
    /// boot-canonical session id; empty path for builds that wire neither.
    transcript_path: PathBuf,
}

impl OrchestratorTaskCreatedFirer {
    /// Build a firer over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator so the `TaskCreated` hook rides the identical registry /
    /// async / sandbox plumbing.
    #[must_use]
    pub fn new(hooks: Arc<HookExecutorImpl>, cwd: PathBuf, transcript_path: PathBuf) -> Self {
        Self {
            hooks,
            cwd,
            transcript_path,
        }
    }
}

#[async_trait]
impl TaskCreatedFirer for OrchestratorTaskCreatedFirer {
    async fn fire(&self, fire: TaskCreatedFire) {
        // The `HookEvent::TaskCreated` variant names its subject field
        // `task_type` (the taxonomy bucket the envelope builder serializes as
        // the wire `task_subject`) and its detail field `description` (wire
        // `task_description`). Map the fire's `task_subject` / `task_description`
        // onto those, and propagate the fire's `teammate_name` / `team_name`
        // (TS `getAgentName()` / `getTeamName()`, `TaskCreateTool.ts:97-98`).
        let event = HookEvent::TaskCreated {
            task_id: fire.task_id,
            task_type: fire.task_subject,
            description: fire.task_description.unwrap_or_default(),
            teammate_name: fire.teammate_name,
            team_name: fire.team_name,
        };
        // Context-light: a task-creation transition has no live per-turn session
        // here, so we thread the engine cwd (also the LINGXI_PROJECT_DIR fallback)
        // and the main session's `transcript_path` (FIX B — claude-code's
        // `createBaseHookInput` ALWAYS stamps it). Everything else defaults —
        // matching the `OrchestratorTaskCompletedFirer`.
        let ctx = HookContext {
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            ..Default::default()
        };
        // Best-effort: the executor never errors out of `execute`, so a
        // misbehaving / absent hook degrades to a no-op and never breaks the
        // caller's task creation. The aggregate result is intentionally dropped
        // — `TaskCreated` is observational here (the Rust registry has no
        // rollback path for a blocking hook).
        let _ = self.hooks.execute(event, ctx).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::noop_hook_executor;

    #[tokio::test]
    async fn no_matching_hook_is_a_noop_fire() {
        // An executor with an empty registry never intervenes => the fire is a
        // silent no-op (the "no hook registered" contract).
        let firer = OrchestratorTaskCreatedFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        // Must not panic / hang.
        firer
            .fire(TaskCreatedFire {
                task_id: "t1".into(),
                task_subject: "LocalBash".into(),
                task_description: Some("do the work".into()),
                teammate_name: None,
                team_name: None,
            })
            .await;
    }
}
