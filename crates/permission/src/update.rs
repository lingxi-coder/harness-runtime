//! Permission update DTO — used to record rule changes.

use crate::result::PermissionUpdateDestination;
use crate::rule::PermissionRule;

/// One pending update to the permission rule set.
#[derive(Debug, Clone)]
pub struct PermissionUpdate {
    /// The rule to add/modify.
    pub rule: PermissionRule,
    /// Where to persist it.
    pub destination: PermissionUpdateDestination,
}
