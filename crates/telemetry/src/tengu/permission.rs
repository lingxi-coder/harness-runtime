//! Permission-flow events. Appended as their own registry block (after the
//! config-migration block).

/// `tengu_bypass_permissions_mode_dialog_accept`
/// (`BypassPermissionsModeDialog.tsx` accept branch).
pub const BYPASS_PERMISSIONS_MODE_DIALOG_ACCEPT: &str =
    "tengu_bypass_permissions_mode_dialog_accept";

/// Registry block.
pub const NAMES: [&str; 1] = [BYPASS_PERMISSIONS_MODE_DIALOG_ACCEPT];
