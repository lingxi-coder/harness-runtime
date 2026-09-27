//! Coordinator / multi-agent swarm events.
//!
//! 1:1 with the claude-code coordinator emit sites:
//! - `tengu_team_created` (`TeamCreateTool.ts:214`)
//! - `tengu_team_deleted` (`TeamDeleteTool.ts:111`)
//! - `tengu_coordinator_mode_switched` (`coordinatorMode.ts:71`,
//!   fired by `matchSessionMode` on a resume mode-mismatch flip)
//!
//! These are distinct from the in-tree `tengu_tool_team_*` lifecycle events
//! (`tengu::tool`): those track the in-tree `team-mem` builtin's tool
//! invocation phases, whereas these mirror claude-code's coordinator swarm
//! analytics. The block is appended at the GLOBAL TAIL of
//! [`crate::tengu::ALL_EVENT_NAMES`] (after the permission block) so every
//! pre-existing per-block prefix slice is preserved (append-only, spec §7
//! line 787-789).

/// `tengu_team_created` — a coordinator created a team (one team per leader).
pub const TEAM_CREATED: &str = "tengu_team_created";
/// `tengu_team_deleted` — a coordinator disbanded a team and cleaned up dirs.
pub const TEAM_DELETED: &str = "tengu_team_deleted";
/// `tengu_coordinator_mode_switched` — resume reconciliation flipped the
/// coordinator-mode flag to match the resumed session.
pub const MODE_SWITCHED: &str = "tengu_coordinator_mode_switched";

/// Registry block — order is locked (append-only). Consumed by
/// [`crate::tengu::ALL_EVENT_NAMES`].
pub const NAMES: [&str; 3] = [TEAM_CREATED, TEAM_DELETED, MODE_SWITCHED];
