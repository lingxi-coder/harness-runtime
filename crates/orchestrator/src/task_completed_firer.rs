//! Orchestrator-side [`hooks::TaskCompletedFirer`] implementation.
//!
//! The `tasks` crate is a LEAF: it owns the task lifecycle
//! ([`tasks::registry::TaskRegistry::set_status`]) but has no access to a live
//! hook executor, and the orchestrator depends on `hooks` but NOT on `tasks`.
//! The firer trait therefore lives in `hooks` (both crates can name it); this
//! adapter — mirroring [`OrchestratorHookDispatcher`] (the MCP elicitation
//! firer) — closes the seam from the orchestrator side: it owns the engine's
//! `Arc<hooks::HookExecutorImpl>` (`orch.hooks`) plus the engine cwd,
//! translates a [`hooks::TaskCompletedFire`] into a
//! [`hooks::HookEvent::TaskCompleted`], and fires the registry best-effort.
//!
//! Parity: reproduces claude-code's `executeTaskCompletedHooks`
//! (`utils/hooks.ts`) — the wire payload (`TaskCompletedHookInputSchema`,
//! `coreSchemas.ts:614-625`) carries `task_id` / `task_subject` /
//! `task_description` / `teammate_name` / `team_name`. The `status` on the fire
//! is routing only (the schema has no `status` field).
//!
//! Best-effort: [`HookExecutorImpl::execute`] never errors out, so a failing or
//! absent `TaskCompleted` hook degrades to a no-op and never breaks the task's
//! status transition — matching the `subagent_stop` / `stop_hooks` arms.
//!
//! Wiring: the composition root (`engine-desktop`) builds an
//! [`OrchestratorTaskCompletedFirer`] over the SAME `Arc<HookExecutorImpl>` it
//! hands the orchestrator, then injects it via
//! `TaskRegistry::with_task_completed_firer` (an `Option`, default `None` => no
//! fire).
//!
//! [`OrchestratorHookDispatcher`]: crate::OrchestratorHookDispatcher
//! [`tasks::registry::TaskRegistry::set_status`]: ../../tasks/src/registry.rs

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::events::HookEvent;
use hooks::task_completed_firer::{TaskCompletedFire, TaskCompletedFirer};
use hooks::HookExecutorImpl;

/// Adapts the engine's hook executor to the `tasks` crate's `TaskCompleted`
/// firer seam.
pub struct OrchestratorTaskCompletedFirer {
    /// The SAME executor the orchestrator fires its other hooks through.
    hooks: Arc<HookExecutorImpl>,
    context_source: Arc<crate::HookContextSource>,
}

impl OrchestratorTaskCompletedFirer {
    /// Share the composition root's actual session owner, including transitions.
    #[must_use]
    pub fn with_context_source(mut self, source: Arc<crate::HookContextSource>) -> Self {
        self.context_source = source;
        self
    }

    /// Build a firer over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator so the `TaskCompleted` hook rides the identical registry /
    /// async / sandbox plumbing.
    #[must_use]
    pub fn new(hooks: Arc<HookExecutorImpl>, cwd: PathBuf, transcript_path: PathBuf) -> Self {
        Self {
            hooks,
            context_source: Arc::new(crate::HookContextSource::from_paths(cwd, transcript_path)),
        }
    }
}

#[async_trait]
impl TaskCompletedFirer for OrchestratorTaskCompletedFirer {
    async fn fire(&self, fire: TaskCompletedFire) {
        let Some(ctx) = self.context_source.task_completed(&fire.task_id).await else {
            return;
        };
        let event = HookEvent::TaskCompleted {
            task_id: fire.task_id,
            status: fire.status,
            task_subject: fire.task_subject,
            task_description: fire.task_description,
            teammate_name: fire.teammate_name,
            team_name: fire.team_name,
        };
        // The registry captures this task's originating owner before any
        // detached lifecycle job can cross a foreground session transition.
        // Best-effort: the executor never errors out of `execute`, so a
        // misbehaving / absent hook degrades to a no-op and never breaks the
        // caller's status transition. The aggregate result is intentionally
        // dropped — `TaskCompleted` is observational here (claude-code's
        // blocking path lives in `TaskUpdateTool`, not the registry transition).
        let _ = self.hooks.execute(event, ctx).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::noop_hook_executor;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    fn source_string(value: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
            .expect("Rust strings are valid UTF-16 projections")
    }

    #[tokio::test]
    async fn no_matching_hook_is_a_noop_fire() {
        // An executor with an empty registry never intervenes => the fire is a
        // silent no-op (the "no hook registered" contract).
        let firer = OrchestratorTaskCompletedFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        // Must not panic / hang.
        firer
            .fire(TaskCompletedFire {
                task_id: "t1".into(),
                status: "completed".into(),
                task_subject: source_string("ship it"),
                task_description: Some(source_string("do the work")),
                teammate_name: None,
                team_name: None,
            })
            .await;
    }
}
