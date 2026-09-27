//! Orchestrator-side [`hooks::TeammateIdleFirer`] implementation.
//!
//! The `tasks` crate is a LEAF: the
//! [`InProcessTeammateHandler`](../../tasks/src/handlers/in_process_teammate.rs)
//! owns the teammate lifecycle (its streaming worker observes the persistent
//! runner's per-turn-set `Completed`, at which point the teammate parks awaiting
//! the next message — "about to go idle") but has no access to a live hook
//! executor, and the orchestrator depends on `hooks` but NOT on `tasks`. The
//! firer trait therefore lives in `hooks` (both crates can name it); this
//! adapter — mirroring [`OrchestratorTaskCompletedFirer`] — closes the seam from
//! the orchestrator side: it owns the engine's `Arc<hooks::HookExecutorImpl>`
//! (`orch.hooks`) plus the engine cwd, translates a
//! [`hooks::TeammateIdleFire`] into a [`hooks::HookEvent::TeammateIdle`], and
//! fires the handler best-effort.
//!
//! Parity: reproduces claude-code's `executeTeammateIdleHooks`
//! (`utils/hooks.ts:3709`), fired from `stopHooks.ts:403` when a teammate's
//! query loop stops — the wire payload (`TeammateIdleHookInputSchema`,
//! `coreSchemas.ts:591-598`) carries `teammate_name` + `team_name` (both
//! required).
//!
//! Best-effort: [`HookExecutorImpl::execute`] never errors out, so a failing or
//! absent `TeammateIdle` hook degrades to a no-op and never disturbs the
//! teammate's turn-set loop — matching the `subagent_stop` / `task_completed`
//! arms.
//!
//! Wiring: the composition root (`engine-desktop`) builds an
//! [`OrchestratorTeammateIdleFirer`] over the SAME `Arc<HookExecutorImpl>` it
//! hands the orchestrator, then injects it via
//! `InProcessTeammateHandler::with_teammate_idle_firer` (an `Option`, default
//! `None` => no fire).
//!
//! [`OrchestratorTaskCompletedFirer`]: crate::OrchestratorTaskCompletedFirer

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use hooks::teammate_idle_firer::{TeammateIdleFire, TeammateIdleFirer, TeammateIdleOutcome};
use hooks::HookExecutorImpl;

/// Adapts the engine's hook executor to the `tasks` crate's `TeammateIdle` firer
/// seam.
pub struct OrchestratorTeammateIdleFirer {
    /// The SAME executor the orchestrator fires its other hooks through.
    hooks: Arc<HookExecutorImpl>,
    /// Engine cwd, threaded into the `TeammateIdle` hook payload (`cwd`) and the
    /// per-hook Command-arm `LINGXI_PROJECT_DIR` fallback.
    cwd: PathBuf,
    /// The MAIN orchestrator session's transcript path
    /// (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
    /// `getTranscriptPathForSession`), stamped on the `TeammateIdle` hook payload's
    /// `transcript_path` (FIX B). Empty for builds wiring neither.
    transcript_path: PathBuf,
}

impl OrchestratorTeammateIdleFirer {
    /// Build a firer over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator so the `TeammateIdle` hook rides the identical registry /
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
impl TeammateIdleFirer for OrchestratorTeammateIdleFirer {
    async fn fire(&self, fire: TeammateIdleFire) -> TeammateIdleOutcome {
        let event = HookEvent::TeammateIdle {
            teammate_name: fire.teammate_name,
            team_name: fire.team_name,
        };
        // E_n uses the owning session and the live permission mode, but no
        // tool-use context (therefore no agent_id or effort).
        let ctx = HookContext {
            session_id: fire.session_id,
            permission_mode: Some(fire.permission_mode),
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            ..Default::default()
        };
        let aggregate = self.hooks.execute(event, ctx).await;
        let blocking_feedback = if matches!(
            aggregate.decision,
            Some(hooks::response::HookDecision::Block)
        ) {
            vec![format!(
                "TeammateIdle hook feedback:\n{}",
                aggregate.reason.as_deref().unwrap_or("Blocked by hook")
            )]
        } else {
            Vec::new()
        };
        TeammateIdleOutcome {
            prevent_continuation: aggregate.prevent_continuation,
            reason: aggregate.prevent_continuation.then(|| {
                aggregate
                    .reason
                    .unwrap_or_else(|| "TeammateIdle hook prevented continuation".to_string())
            }),
            blocking_feedback,
            additional_contexts: aggregate.additional_contexts,
        }
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
        let firer = OrchestratorTeammateIdleFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        // Must not panic / hang.
        let outcome = firer
            .fire(TeammateIdleFire {
                session_id: protocol::SessionId::nil(),
                permission_mode: "default".into(),
                teammate_name: "buddy".into(),
                team_name: "alpha".into(),
            })
            .await;
        assert_eq!(outcome, TeammateIdleOutcome::default());
    }
}
