//! `tengu_fusion_*` event name constants.
//!
//! Like `workflow`, these are string-locked but intentionally excluded from the
//! count-locked `ALL_EVENT_NAMES` snapshot.

/// Fusion run accepted and panel set resolved.
pub const STARTED: &str = "tengu_fusion_started";
/// One panel subagent started.
pub const PANEL_STARTED: &str = "tengu_fusion_panel_started";
/// One panel finished successfully.
pub const PANEL_COMPLETED: &str = "tengu_fusion_panel_completed";
/// One panel finished unsuccessfully.
pub const PANEL_FAILED: &str = "tengu_fusion_panel_failed";
/// Analyst phase finished successfully.
pub const ANALYSIS_COMPLETED: &str = "tengu_fusion_analysis_completed";
/// Analyst phase failed.
pub const ANALYSIS_FAILED: &str = "tengu_fusion_analysis_failed";
/// Synthesizer phase finished successfully.
pub const SYNTHESIS_COMPLETED: &str = "tengu_fusion_synthesis_completed";
/// Synthesizer phase failed.
pub const SYNTHESIS_FAILED: &str = "tengu_fusion_synthesis_failed";
/// Fusion reached a terminal completed/needs-parent result.
pub const COMPLETED: &str = "tengu_fusion_completed";
/// Fusion failed before a usable result.
pub const FAILED: &str = "tengu_fusion_failed";
/// Fusion was cancelled.
pub const CANCELLED: &str = "tengu_fusion_cancelled";

/// Complete event-name set for Fusion telemetry.
pub const NAMES: &[&str] = &[
    STARTED,
    PANEL_STARTED,
    PANEL_COMPLETED,
    PANEL_FAILED,
    ANALYSIS_COMPLETED,
    ANALYSIS_FAILED,
    SYNTHESIS_COMPLETED,
    SYNTHESIS_FAILED,
    COMPLETED,
    FAILED,
    CANCELLED,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_names_are_tengu_fusion_prefixed() {
        for &name in NAMES {
            assert!(
                name.starts_with("tengu_fusion_"),
                "fusion event name {name:?} does not start with 'tengu_fusion_'"
            );
        }
    }

    #[test]
    fn fusion_event_name_set_is_complete() {
        assert_eq!(
            NAMES,
            &[
                "tengu_fusion_started",
                "tengu_fusion_panel_started",
                "tengu_fusion_panel_completed",
                "tengu_fusion_panel_failed",
                "tengu_fusion_analysis_completed",
                "tengu_fusion_analysis_failed",
                "tengu_fusion_synthesis_completed",
                "tengu_fusion_synthesis_failed",
                "tengu_fusion_completed",
                "tengu_fusion_failed",
                "tengu_fusion_cancelled",
            ]
        );
    }
}
