//! `PermissionMode` — top-level authorization mode that gates unmatched tool calls.
//!
//! Five modes are external (settable from `settings.json` and CLI flags) and
//! one is internal-only (`Bubble`) and one is CLI-settable but still rejected
//! from persistent settings until the settings schema grows auto-mode metadata
//! (`Auto`).

use serde::{Deserialize, Serialize};

/// The active permission mode applied when no rule matches a tool call.
///
/// `Default` asks the user, `Plan` restricts to read-only operations,
/// `AcceptEdits` auto-allows file edits, `BypassPermissions` allows
/// everything (unless the killswitch is set), and `DontAsk` denies
/// everything that does not have an explicit allow rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    // External / user-addressable (settings.json + CLI accept these).
    /// Ask the user before each unmatched tool call.
    Default,
    /// Plan-only mode: read-only operations and planning, no mutations.
    Plan,
    /// Auto-accept file edits without prompting.
    AcceptEdits,
    /// Allow everything (subject to the bypass killswitch).
    BypassPermissions,
    /// Deny everything that lacks an explicit allow rule.
    DontAsk,
    // Internal engine modes.
    /// Internal: bubble the decision to a parent agent.
    Bubble,
    /// Classifier-driven auto-accept with denial-tracking fallback. CLI-settable;
    /// persistent settings still reject it as non-external.
    Auto,
}

impl PermissionMode {
    /// Returns true when this mode may appear in user-facing configuration.
    ///
    /// External modes are accepted by `settings.json` and CLI flag validation.
    /// Internal modes (`Bubble`, `Auto`) are rejected as input but produced
    /// by the engine at runtime.
    #[must_use]
    pub fn is_external(self) -> bool {
        !matches!(self, Self::Bubble | Self::Auto)
    }

    /// The human-facing mode title shown in permission prompts, 1:1 with
    /// claude-code's `permissionModeTitle` → `getModeConfig(mode).title`
    /// (`PermissionMode.ts:46-83`). `Bubble` has no claude-code config entry
    /// (internal-only, never user-displayed) and falls back to `Default`.
    #[must_use]
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Default | Self::Bubble => "Manual",
            Self::Plan => "Plan",
            Self::AcceptEdits => "Accept edits",
            Self::BypassPermissions => "Bypass Permissions",
            Self::DontAsk => "Don't Ask",
            Self::Auto => "Auto",
        }
    }

    /// The wire string form (claude-code `PermissionMode.ts`) — the inverse of
    /// [`crate::permission_mode_from_cli_string`]. `Bubble` (internal-only, no
    /// wire form) maps to `default`. Used to snapshot the live mode across the
    /// in-process `/resume` re-mount and for the engine `set_permission_mode`
    /// control request.
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Default | Self::Bubble => "default",
            Self::Plan => "plan",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
        }
    }
}

/// The next mode in the Shift+Tab UI cycle (claude-code `fJn`, binary v2.1.183
/// at offset ~206604306):
///
/// ```text
/// switch(e.mode){
///   case"default":            return"acceptEdits";
///   case"acceptEdits":        return"plan";
///   case"plan":               if(e.isBypassPermissionsModeAvailable) return"bypassPermissions";
///                             if(mJn(e)) return"auto";
///                             return"default";
///   case"bypassPermissions":  if(mJn(e)) return"auto";
///                             return"default";
///   case"dontAsk":            return"default";
///   default:                  return"default";
/// }
/// ```
///
/// So `Default→AcceptEdits→Plan→…`, where `Plan` advances to `BypassPermissions`
/// when `bypass_available`, else to `Auto` when `can_cycle_to_auto`, else
/// `Default`; `BypassPermissions` advances to `Auto` when `can_cycle_to_auto`,
/// else `Default`; `DontAsk`/internal modes return `Default`.
///
/// `can_cycle_to_auto` is the `mJn(e)` gate, NOT a `USER_TYPE==='ant'` check.
/// In the binary `mJn(e) = !!e.isAutoModeAvailable && bx() && !GPo()`, where
/// `bx()` requires `!isAutoModeCircuitBroken() && disableAutoMode !== "disable"
/// && model-gate XCe(js())`, and `GPo()` is `autoModeOptInDismissed && !nDe()`.
/// None of those signals exist in this port yet, so callers currently pass
/// `false` (byte-faithful: when the gate is unavailable, `Plan`/`BypassPermissions`
/// fall through to `Default` exactly as `fJn` does).
#[must_use]
pub fn next_permission_mode(
    current: PermissionMode,
    bypass_available: bool,
    can_cycle_to_auto: bool,
) -> PermissionMode {
    use PermissionMode::{AcceptEdits, Auto, BypassPermissions, Default, Plan};
    match current {
        Default => AcceptEdits,
        AcceptEdits => Plan,
        // `plan`: bypass first, then the auto gate, else default.
        Plan if bypass_available => BypassPermissions,
        Plan if can_cycle_to_auto => Auto,
        // `bypassPermissions`: auto gate, else default.
        BypassPermissions if can_cycle_to_auto => Auto,
        // Plan (no bypass, no auto), BypassPermissions (no auto), DontAsk,
        // Bubble, Auto → Default.
        _ => Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubble_is_internal() {
        assert!(!PermissionMode::Bubble.is_external());
    }

    #[test]
    fn default_is_external() {
        assert!(PermissionMode::Default.is_external());
    }

    #[test]
    fn titles_match_2_1_211_mode_config_map() {
        // MODE-TITLE-BYTES-05: the 2.1.211 `tyl` map gives plan.title="Plan"
        // and auto.title="Auto" (NOT "Plan Mode"/"Auto mode"); these titles are
        // interpolated verbatim into the `Current permission mode (${title})
        // requires approval for this ${tool} command` ask message.
        assert_eq!(PermissionMode::Default.title(), "Manual");
        assert_eq!(PermissionMode::Bubble.title(), "Manual");
        assert_eq!(PermissionMode::Plan.title(), "Plan");
        assert_eq!(PermissionMode::AcceptEdits.title(), "Accept edits");
        assert_eq!(
            PermissionMode::BypassPermissions.title(),
            "Bypass Permissions"
        );
        assert_eq!(PermissionMode::DontAsk.title(), "Don't Ask");
        assert_eq!(PermissionMode::Auto.title(), "Auto");
    }

    #[test]
    fn cycle_external_path_matches_ts() {
        use PermissionMode::{AcceptEdits, BypassPermissions, Default, DontAsk, Plan};
        // Gate OFF (can_cycle_to_auto = false) — the pre-#33 behavior, unchanged.
        // bypass NOT available
        assert_eq!(next_permission_mode(Default, false, false), AcceptEdits);
        assert_eq!(next_permission_mode(AcceptEdits, false, false), Plan);
        assert_eq!(next_permission_mode(Plan, false, false), Default);
        assert_eq!(
            next_permission_mode(BypassPermissions, false, false),
            Default
        );
        assert_eq!(next_permission_mode(DontAsk, false, false), Default);
        // bypass available → Plan advances to BypassPermissions
        assert_eq!(next_permission_mode(Plan, true, false), BypassPermissions);
        assert_eq!(next_permission_mode(Default, true, false), AcceptEdits); // unchanged
        assert_eq!(
            next_permission_mode(BypassPermissions, true, false),
            Default
        );
        // internal modes fall back to Default
        assert_eq!(
            next_permission_mode(PermissionMode::Bubble, true, false),
            Default
        );
        assert_eq!(
            next_permission_mode(PermissionMode::Auto, true, false),
            Default
        );
    }

    #[test]
    fn cycle_to_auto_matches_fjn() {
        use PermissionMode::{AcceptEdits, Auto, BypassPermissions, Default, DontAsk, Plan};
        // `fJn` case"plan": bypass is checked BEFORE the auto gate, so when both
        // are available bypass wins.
        assert_eq!(next_permission_mode(Plan, true, true), BypassPermissions);
        // `plan` with no bypass but auto available → Auto.
        assert_eq!(next_permission_mode(Plan, false, true), Auto);
        // `bypassPermissions` with the auto gate → Auto.
        assert_eq!(next_permission_mode(BypassPermissions, false, true), Auto);
        assert_eq!(next_permission_mode(BypassPermissions, true, true), Auto);
        // The auto gate does NOT alter the head of the cycle.
        assert_eq!(next_permission_mode(Default, false, true), AcceptEdits);
        assert_eq!(next_permission_mode(AcceptEdits, false, true), Plan);
        // `dontAsk` and internal modes still go to Default even with the gate on.
        assert_eq!(next_permission_mode(DontAsk, false, true), Default);
        assert_eq!(
            next_permission_mode(PermissionMode::Bubble, false, true),
            Default
        );
        assert_eq!(next_permission_mode(Auto, false, true), Default);
    }
}
