//! `tengu_command_<name>_<phase>` event names — the M5-10 batch-1 + M5-11 batch-2 surface.
//!
//! M5-10 ships 18 events (6 commands × 3 phases). M5-11 appends another 36
//! (12 batch-2 commands × 3 phases) for a total of **54**. See plans
//! `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md` (Task 1) and
//! `docs/superpowers/plans/2026-05-25-m5-11-commands-batch-2.md` (Task 1).

/// Emitted when `/clear` begins executing.
pub const CLEAR_STARTED: &str = "tengu_command_clear_started";
/// Emitted when `/clear` succeeds.
pub const CLEAR_COMPLETED: &str = "tengu_command_clear_completed";
/// Emitted when `/clear` fails.
pub const CLEAR_FAILED: &str = "tengu_command_clear_failed";

/// Emitted when `/compact` begins executing.
pub const COMPACT_STARTED: &str = "tengu_command_compact_started";
/// Emitted when `/compact` succeeds.
pub const COMPACT_COMPLETED: &str = "tengu_command_compact_completed";
/// Emitted when `/compact` fails.
pub const COMPACT_FAILED: &str = "tengu_command_compact_failed";

/// Emitted when `/exit` begins executing.
pub const EXIT_STARTED: &str = "tengu_command_exit_started";
/// Emitted when `/exit` succeeds (always — `request_exit` is infallible).
pub const EXIT_COMPLETED: &str = "tengu_command_exit_completed";
/// Emitted when `/exit` fails (currently unreachable, reserved for future).
pub const EXIT_FAILED: &str = "tengu_command_exit_failed";

/// Emitted when `/help` begins executing.
pub const HELP_STARTED: &str = "tengu_command_help_started";
/// Emitted when `/help` succeeds.
pub const HELP_COMPLETED: &str = "tengu_command_help_completed";
/// Emitted when `/help` fails (currently unreachable, reserved for future).
pub const HELP_FAILED: &str = "tengu_command_help_failed";

/// Emitted when `/init` begins executing.
pub const INIT_STARTED: &str = "tengu_command_init_started";
/// Emitted when `/init` succeeds (template injected as next user message).
pub const INIT_COMPLETED: &str = "tengu_command_init_completed";
/// Emitted when `/init` fails (currently unreachable, reserved for future).
pub const INIT_FAILED: &str = "tengu_command_init_failed";

/// Emitted when `/memory` begins executing (before spawning `$EDITOR`).
pub const MEMORY_STARTED: &str = "tengu_command_memory_started";
/// Emitted when `/memory` succeeds (after `$EDITOR` exits, regardless of code).
pub const MEMORY_COMPLETED: &str = "tengu_command_memory_completed";
/// Emitted when `/memory` fails (editor spawn / target-path I/O failure).
pub const MEMORY_FAILED: &str = "tengu_command_memory_failed";

// ────────────────────────────────────────────────────────────────────────────
// M5-11 batch 2: 12 commands × 3 phases = 36 new events
// ────────────────────────────────────────────────────────────────────────────

/// Emitted when `/agents` begins executing.
pub const AGENTS_STARTED: &str = "tengu_command_agents_started";
/// Emitted when `/agents` succeeds.
pub const AGENTS_COMPLETED: &str = "tengu_command_agents_completed";
/// Emitted when `/agents` fails.
pub const AGENTS_FAILED: &str = "tengu_command_agents_failed";

/// Emitted when `/config` begins executing.
pub const CONFIG_STARTED: &str = "tengu_command_config_started";
/// Emitted when `/config` succeeds.
pub const CONFIG_COMPLETED: &str = "tengu_command_config_completed";
/// Emitted when `/config` fails.
pub const CONFIG_FAILED: &str = "tengu_command_config_failed";

/// Emitted when `/cost` begins executing.
pub const COST_STARTED: &str = "tengu_command_cost_started";
/// Emitted when `/cost` succeeds.
pub const COST_COMPLETED: &str = "tengu_command_cost_completed";
/// Emitted when `/cost` fails.
pub const COST_FAILED: &str = "tengu_command_cost_failed";

/// Emitted when `/doctor` begins executing.
pub const DOCTOR_STARTED: &str = "tengu_command_doctor_started";
/// Emitted when `/doctor` succeeds.
pub const DOCTOR_COMPLETED: &str = "tengu_command_doctor_completed";
/// Emitted when `/doctor` fails.
pub const DOCTOR_FAILED: &str = "tengu_command_doctor_failed";

/// Emitted when `/hooks` begins executing.
pub const HOOKS_STARTED: &str = "tengu_command_hooks_started";
/// Emitted when `/hooks` succeeds.
pub const HOOKS_COMPLETED: &str = "tengu_command_hooks_completed";
/// Emitted when `/hooks` fails.
pub const HOOKS_FAILED: &str = "tengu_command_hooks_failed";

/// Emitted when `/login` begins executing.
pub const LOGIN_STARTED: &str = "tengu_command_login_started";
/// Emitted when `/login` succeeds.
pub const LOGIN_COMPLETED: &str = "tengu_command_login_completed";
/// Emitted when `/login` fails.
pub const LOGIN_FAILED: &str = "tengu_command_login_failed";

/// Emitted when `/logout` begins executing.
pub const LOGOUT_STARTED: &str = "tengu_command_logout_started";
/// Emitted when `/logout` succeeds.
pub const LOGOUT_COMPLETED: &str = "tengu_command_logout_completed";
/// Emitted when `/logout` fails.
pub const LOGOUT_FAILED: &str = "tengu_command_logout_failed";

/// Emitted when `/mcp` begins executing.
pub const MCP_STARTED: &str = "tengu_command_mcp_started";
/// Emitted when `/mcp` succeeds.
pub const MCP_COMPLETED: &str = "tengu_command_mcp_completed";
/// Emitted when `/mcp` fails.
pub const MCP_FAILED: &str = "tengu_command_mcp_failed";

/// Emitted when `/model` begins executing.
pub const MODEL_STARTED: &str = "tengu_command_model_started";
/// Emitted when `/model` succeeds.
pub const MODEL_COMPLETED: &str = "tengu_command_model_completed";
/// Emitted when `/model` fails.
pub const MODEL_FAILED: &str = "tengu_command_model_failed";

/// Emitted when `/permissions` begins executing.
pub const PERMISSIONS_STARTED: &str = "tengu_command_permissions_started";
/// Emitted when `/permissions` succeeds.
pub const PERMISSIONS_COMPLETED: &str = "tengu_command_permissions_completed";
/// Emitted when `/permissions` fails.
pub const PERMISSIONS_FAILED: &str = "tengu_command_permissions_failed";

/// Emitted when `/status` begins executing.
pub const STATUS_STARTED: &str = "tengu_command_status_started";
/// Emitted when `/status` succeeds.
pub const STATUS_COMPLETED: &str = "tengu_command_status_completed";
/// Emitted when `/status` fails.
pub const STATUS_FAILED: &str = "tengu_command_status_failed";

/// Emitted when `/version` begins executing.
pub const VERSION_STARTED: &str = "tengu_command_version_started";
/// Emitted when `/version` succeeds.
pub const VERSION_COMPLETED: &str = "tengu_command_version_completed";
/// Emitted when `/version` fails (currently unreachable, reserved for future).
pub const VERSION_FAILED: &str = "tengu_command_version_failed";

/// The flat `tengu_cd_command` event claude-code 2.1.207 emits after a `/cd`
/// working-directory move (`N("tengu_cd_command", { source })`). Deliberately
/// NOT one of the `tengu_command_<name>_<phase>` names in [`NAMES`] (the `/cd`
/// interactive local-jsx command does not use the started/completed/failed
/// lifecycle triple) — so it is excluded from that locked array.
pub const CD_COMMAND: &str = "tengu_cd_command";

/// All 54 command-event names (sorted ASCII-ascending: by command-name then phase).
///
/// Locked at length **54** for M5-11 ([`super::ALL_EVENT_NAMES`] formula must
/// add **54** for the `command` slot).
///
/// Sort: per-command alphabetical; within each command the phase order is
/// `_completed < _failed < _started` (ASCII: `c` < `f` < `s`).
pub const NAMES: &[&str; 54] = &[
    AGENTS_COMPLETED,
    AGENTS_FAILED,
    AGENTS_STARTED,
    CLEAR_COMPLETED,
    CLEAR_FAILED,
    CLEAR_STARTED,
    COMPACT_COMPLETED,
    COMPACT_FAILED,
    COMPACT_STARTED,
    CONFIG_COMPLETED,
    CONFIG_FAILED,
    CONFIG_STARTED,
    COST_COMPLETED,
    COST_FAILED,
    COST_STARTED,
    DOCTOR_COMPLETED,
    DOCTOR_FAILED,
    DOCTOR_STARTED,
    EXIT_COMPLETED,
    EXIT_FAILED,
    EXIT_STARTED,
    HELP_COMPLETED,
    HELP_FAILED,
    HELP_STARTED,
    HOOKS_COMPLETED,
    HOOKS_FAILED,
    HOOKS_STARTED,
    INIT_COMPLETED,
    INIT_FAILED,
    INIT_STARTED,
    LOGIN_COMPLETED,
    LOGIN_FAILED,
    LOGIN_STARTED,
    LOGOUT_COMPLETED,
    LOGOUT_FAILED,
    LOGOUT_STARTED,
    MCP_COMPLETED,
    MCP_FAILED,
    MCP_STARTED,
    MEMORY_COMPLETED,
    MEMORY_FAILED,
    MEMORY_STARTED,
    MODEL_COMPLETED,
    MODEL_FAILED,
    MODEL_STARTED,
    PERMISSIONS_COMPLETED,
    PERMISSIONS_FAILED,
    PERMISSIONS_STARTED,
    STATUS_COMPLETED,
    STATUS_FAILED,
    STATUS_STARTED,
    VERSION_COMPLETED,
    VERSION_FAILED,
    VERSION_STARTED,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_length_locked_at_54() {
        assert_eq!(NAMES.len(), 54);
    }

    #[test]
    fn batch_2_extends_to_54_total() {
        assert_eq!(
            NAMES.len(),
            54,
            "M5-11 should grow command::NAMES from 18 to 54"
        );
    }

    #[test]
    fn names_are_sorted_ascii_ascending() {
        let mut sorted = NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(NAMES, sorted.as_slice());
    }

    #[test]
    fn every_name_starts_with_tengu_command() {
        for n in NAMES {
            assert!(n.starts_with("tengu_command_"), "{n} missing prefix");
        }
    }

    #[test]
    fn every_phase_is_started_completed_or_failed() {
        for n in NAMES {
            assert!(
                n.ends_with("_started") || n.ends_with("_completed") || n.ends_with("_failed"),
                "{n} has wrong phase"
            );
        }
    }

    #[test]
    fn exactly_three_phases_per_command() {
        let mut by_cmd: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for n in NAMES {
            let stripped = n.strip_prefix("tengu_command_").unwrap();
            let last_under = stripped.rfind('_').expect("phase delimiter missing");
            let cmd = &stripped[..last_under];
            by_cmd.entry(cmd).or_default().push(n);
        }
        assert_eq!(by_cmd.len(), 18, "expected 18 distinct command names");
        for (cmd, names) in &by_cmd {
            assert_eq!(
                names.len(),
                3,
                "/{cmd} should have exactly 3 phases, got {}",
                names.len()
            );
        }
    }

    #[test]
    fn covers_all_6_batch_1_commands() {
        let expected: std::collections::HashSet<&str> =
            ["clear", "compact", "exit", "help", "init", "memory"]
                .iter()
                .copied()
                .collect();
        let mut found: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for n in NAMES {
            let stripped = n.strip_prefix("tengu_command_").unwrap();
            let last_under = stripped.rfind('_').unwrap();
            let cmd = &stripped[..last_under];
            if expected.contains(cmd) {
                found.insert(cmd);
            }
        }
        assert_eq!(found, expected, "missing batch-1 command names");
    }

    #[test]
    fn batch_2_covers_all_12_extra_commands() {
        let expected: std::collections::HashSet<&str> = [
            "agents",
            "config",
            "cost",
            "doctor",
            "hooks",
            "login",
            "logout",
            "mcp",
            "model",
            "permissions",
            "status",
            "version",
        ]
        .iter()
        .copied()
        .collect();
        let mut found: std::collections::HashSet<&str> = std::collections::HashSet::default();
        for n in NAMES {
            let stripped = n.strip_prefix("tengu_command_").unwrap();
            let last_under = stripped.rfind('_').unwrap();
            let cmd = &stripped[..last_under];
            if !["clear", "compact", "exit", "help", "init", "memory"].contains(&cmd) {
                found.insert(cmd);
            }
        }
        assert_eq!(found, expected);
    }

    #[test]
    fn individual_constants_match_event_strings() {
        assert_eq!(CLEAR_STARTED, "tengu_command_clear_started");
        assert_eq!(CLEAR_COMPLETED, "tengu_command_clear_completed");
        assert_eq!(CLEAR_FAILED, "tengu_command_clear_failed");
        assert_eq!(INIT_FAILED, "tengu_command_init_failed");
        assert_eq!(MEMORY_FAILED, "tengu_command_memory_failed");
        assert_eq!(COST_STARTED, "tengu_command_cost_started");
        assert_eq!(STATUS_COMPLETED, "tengu_command_status_completed");
        assert_eq!(DOCTOR_FAILED, "tengu_command_doctor_failed");
        assert_eq!(LOGIN_STARTED, "tengu_command_login_started");
        assert_eq!(VERSION_COMPLETED, "tengu_command_version_completed");
    }
}
