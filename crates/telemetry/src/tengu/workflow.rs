//! `tengu_workflow_*` event name constants.
//!
//! These are NOT added to the count-locked `ALL_EVENT_NAMES` / `tengu_events.json`
//! fixture (that snapshot is from an OLDER claude event set; adding these would
//! break the 363-entry byte-parity lock). They live here for string-lock testing,
//! mirroring how `agent::AGENT_TOOL_NAMES` is kept apart.

/// `tengu_workflow_launched` — workflow invoked.
pub const LAUNCHED: &str = "tengu_workflow_launched";
/// `tengu_workflow_completed` — workflow run finished.
pub const COMPLETED: &str = "tengu_workflow_completed";
/// `tengu_workflow_phase_completed` — a phase group finished.
pub const PHASE_COMPLETED: &str = "tengu_workflow_phase_completed";
/// `tengu_workflow_agent_cap_exceeded` — 1000-agent lifetime cap hit.
pub const AGENT_CAP_EXCEEDED: &str = "tengu_workflow_agent_cap_exceeded";
/// `tengu_workflow_budget_cap_exceeded` — token budget ceiling hit.
pub const BUDGET_CAP_EXCEEDED: &str = "tengu_workflow_budget_cap_exceeded";
/// `tengu_workflow_journal_started_hit_respawn` — journal resume re-started a prior agent.
pub const JOURNAL_STARTED_HIT_RESPAWN: &str = "tengu_workflow_journal_started_hit_respawn";
/// `tengu_workflow_saved` — a dynamic workflow was written to disk via the
/// `/workflows` "Save dynamic workflow" dialog (oracle `eya`
/// `M("tengu_workflow_saved", {scope, overwrite, script_size_chars})`).
pub const SAVED: &str = "tengu_workflow_saved";
/// `tengu_workflow_size_warning_shown` — the `/workflows` UI surfaced Claude
/// Code's large-workflow warning for a run.
pub const SIZE_WARNING_SHOWN: &str = "tengu_workflow_size_warning_shown";

/// All 8 reachable workflow telemetry event names (string-lock only, NOT in ALL_EVENT_NAMES).
pub const NAMES: &[&str] = &[
    LAUNCHED,
    COMPLETED,
    PHASE_COMPLETED,
    AGENT_CAP_EXCEEDED,
    BUDGET_CAP_EXCEEDED,
    JOURNAL_STARTED_HIT_RESPAWN,
    SAVED,
    SIZE_WARNING_SHOWN,
];

// Unreachable events (no LingXi trigger):
// - `tengu_workflow_keyword` / `_dismissed` / `_restored` — no keyword UI.
// - `tengu_workflow_usage_warning_accepted` — no usage-warning dialog.
// - `tengu_workflows_enabled` — feature flag read, not an emitted event.

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name in the workflow `NAMES` registry must be a byte-exact
    /// `tengu_workflow_*` string (oracle §1 name set verification).
    #[test]
    fn all_names_are_tengu_workflow_prefixed() {
        for &name in NAMES {
            assert!(
                name.starts_with("tengu_workflow_"),
                "workflow event name {name:?} does not start with 'tengu_workflow_'"
            );
        }
    }
}
