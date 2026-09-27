//! `CoordinatorModeHandle` — narrow trait abstracting
//! `coordinator::CoordinatorMode` so `AgentTool` (and its prompt path) in
//! `lingxi-tools` can consult whether the session is currently in coordinator
//! mode without taking a cyclic dep on `lingxi-coordinator`.
//!
//! Coordinator mode is mutually exclusive with the fork-subagent path
//! (`forkSubagent.ts:34` — the coordinator already owns the orchestration
//! role), and it also selects the slim coordinator tool prompt. `AgentTool`
//! reads this LIVE at spawn / prompt time rather than snapshotting a `bool` at
//! registration, so a mid-session mode switch (resume flip or runtime upgrade)
//! immediately takes effect — matching claude-code's live `isCoordinatorMode()`.
//!
//! Concrete impl lives in `lingxi-coordinator` (`CoordinatorMode`); tests inject
//! a scriptable mock or simply leave the seam `None` (⇒ not coordinator).

/// Live coordinator-mode consultation seam used by `AgentTool`.
pub trait CoordinatorModeHandle: Send + Sync {
    /// Whether coordinator mode is currently active (reads the live flag).
    fn is_enabled(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn CoordinatorModeHandle>> = None;
    }
}
