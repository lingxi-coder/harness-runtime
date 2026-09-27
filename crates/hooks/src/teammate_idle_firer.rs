//! Narrow hook-firer seam for the `TeammateIdle` lifecycle event.
//!
//! The `tasks` crate is a LEAF with no access to a live hook executor: the
//! [`InProcessTeammateHandler`](../../tasks/src/handlers/in_process_teammate.rs)
//! owns the teammate lifecycle (its streaming worker observes the persistent
//! runner's per-turn-set `Completed`, at which point the teammate parks awaiting
//! the next message — "about to go idle") but cannot reach `orch.hooks` without
//! a dependency cycle. This trait closes that seam exactly like
//! [`TaskCompletedFirer`](crate::TaskCompletedFirer): the handler holds an
//! `Option<Arc<dyn TeammateIdleFirer>>` (default `None` => strict no-op) and
//! calls [`fire`](TeammateIdleFirer::fire) each time a turn-set completes and
//! the teammate is about to idle. The narrow [`TeammateIdleOutcome`] return
//! value preserves the only lifecycle signals the caller must act on without
//! leaking the full hook executor aggregate into the leaf task handler.
//!
//! Defining the trait HERE (the `hooks` crate) — rather than in `tasks` — lets
//! the ORCHESTRATOR implement it over its `Arc<HookExecutorImpl>` (the
//! orchestrator depends on `hooks` but NOT on `tasks`, so it could never name a
//! `tasks`-defined trait). The composition root (`engine-desktop`) constructs
//! the impl and injects it where the `InProcessTeammateHandler` is built. This
//! mirrors [`TaskCompletedFirer`] / [`TaskCreatedFirer`](crate::TaskCreatedFirer).
//!
//! Parity: claude-code fires `executeTeammateIdleHooks` (`utils/hooks.ts:3709`)
//! from `stopHooks.ts:403` — after the Stop hooks pass and the teammate's
//! in-progress `TaskCompleted` hooks run — gated on `isTeammate()`, when the
//! teammate's query loop has stopped and it is about to park awaiting the next
//! message. The wire payload (`TeammateIdleHookInputSchema`,
//! `coreSchemas.ts:591-598`) carries `teammate_name` + `team_name` (both
//! required). A blocking hook feeds model-visible feedback back into the
//! teammate so it keeps working; `continue:false` terminates continuation.

use std::sync::Arc;

use async_trait::async_trait;

/// The byte-faithful wire payload for a `TeammateIdle` fire, sourced from the
/// teammate handler at the per-turn-set idle transition. Field set mirrors
/// `TeammateIdleHookInputSchema` (`coreSchemas.ts:591-598`): `teammate_name` +
/// `team_name` are BOTH required. `team_name` rides as `""` when the firing
/// scope (the leaf `tasks` handler) cannot reach the team identity — faithful to
/// claude-code's `getTeamName() ?? ''` fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateIdleFire {
    /// Trusted session owning the teammate, shared with its transcript.
    pub session_id: protocol::SessionId,
    /// Effective live permission mode when the teammate becomes idle.
    pub permission_mode: String,
    /// Name of the teammate going idle (wire `teammate_name`, required).
    pub teammate_name: String,
    /// Team the teammate belongs to (wire `team_name`, required; `""` when the
    /// firing scope has no team identity).
    pub team_name: String,
}

/// Lifecycle-relevant result of running the `TeammateIdle` hook set.
///
/// This is the compact analogue of claude-code's `blockingErrors` plus
/// `preventContinuation` result: blocking feedback and additional context wake
/// the teammate for another turn, while `prevent_continuation` ends the
/// persistent runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeammateIdleOutcome {
    /// `continue:false` from any matching hook. This takes precedence over
    /// blocking feedback and ends the teammate.
    pub prevent_continuation: bool,
    /// Hook-provided `stopReason`, or the oracle default when continuation was
    /// prevented.
    pub reason: Option<String>,
    /// Model-facing blocking feedback. Each entry becomes one meta user
    /// message and therefore drives another turn instead of allowing idle.
    pub blocking_feedback: Vec<String>,
    /// Model-facing `additionalContext` values, in hook execution order.
    pub additional_contexts: Vec<String>,
}

impl TeammateIdleOutcome {
    /// Whether the hook result requires a follow-up model turn.
    ///
    /// `prevent_continuation` is checked FIRST, matching its own contract
    /// ("takes precedence over blocking feedback and ends the teammate"). A hook
    /// that returns both `continue:false` and `decision:"block"` reaches this
    /// type with `prevent_continuation: true` AND a non-empty
    /// `blocking_feedback` (see `OrchestratorTeammateIdleFirer`), so a predicate
    /// that ignored the flag would answer `true` and re-wake a teammate the hook
    /// asked to terminate.
    #[must_use]
    pub fn should_continue_working(&self) -> bool {
        !self.prevent_continuation
            && (!self.blocking_feedback.is_empty() || !self.additional_contexts.is_empty())
    }
}

/// One-method seam the `tasks` crate uses to fire a `TeammateIdle` hook without
/// owning a hook executor. The default handler holds `None` and performs a
/// strict no-op; the orchestrator provides the real impl.
#[async_trait]
pub trait TeammateIdleFirer: Send + Sync {
    /// Fire the `TeammateIdle` hook for a teammate about to park awaiting input.
    ///
    /// Executor failures remain folded into the returned no-op outcome; callers
    /// only act on explicit blocking feedback or `continue:false`.
    async fn fire(&self, fire: TeammateIdleFire) -> TeammateIdleOutcome;
}

/// Convenience alias for the optional firer the
/// [`InProcessTeammateHandler`](../../tasks/src/handlers/in_process_teammate.rs)
/// holds.
pub type OptionalTeammateIdleFirer = Option<Arc<dyn TeammateIdleFirer>>;
