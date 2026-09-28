//! `/cd` — move this session to a new working directory (claude-code 2.1.207
//! `local-jsx` `name:"cd"`, `tengu_cd_command`).
//!
//! `/cd` is interactive: the TUI (`tui::bottom_pane::cd_confirm_view`) shows the
//! byte-exact confirm prompt, and on confirm the CLI effect channel
//! (`apps/cli/src/mode.rs::run_permission_action`) swaps the shared
//! `tool_api::SessionCwd` cell — the SAME cell the `EnterWorktree`/`ExitWorktree`
//! tools swap — then reloads project config for the new cwd (the swap's
//! registered on-swap callback clears the cwd-keyed conditional-rules cache; the
//! `<env>`/gitStatus sections recompute each turn), emits [`emit_command`], and
//! prints [`crate::cd::result_message`].
//!
//! The `command-core` handler surface itself stays a headless
//! `InteractiveOnlyHandler` (registered in [`crate::builtins::register`]) — the real move
//! needs the interactive TUI + the live `SessionCwd`, neither of which a
//! dispatcher-only path has. This module is the shared home for the
//! byte-faithful strings (re-exported from `command-api`) and the analytics
//! emit, so the TUI and CLI sides can never drift.

pub use crate::cd::{result_message, CONFIRM_PROMPT, TELEMETRY_EVENT, TELEMETRY_SOURCE_CD_COMMAND};

/// Emit the `tengu_cd_command` analytics event for a `/cd`-triggered move,
/// mirroring claude-code's `N("tengu_cd_command", { source: xe("cd_command") })`.
pub fn emit_command() {
    telemetry::emit_cd_command(TELEMETRY_SOURCE_CD_COMMAND);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_match_command_api_source() {
        // The byte-exact strings are owned by `crate::cd`; this module only
        // re-exports them so both the TUI and CLI reach one source.
        assert_eq!(CONFIRM_PROMPT, crate::cd::CONFIRM_PROMPT);
        assert_eq!(TELEMETRY_EVENT, "tengu_cd_command");
        assert_eq!(TELEMETRY_SOURCE_CD_COMMAND, "cd_command");
        assert_eq!(
            result_message("/x"),
            "The session's working directory has changed to /x (via /cd). The environment block at the start of this conversation still names the previous directory \u{2014} that information is stale. All tool calls and relative paths now resolve from /x."
        );
    }

    #[test]
    fn emit_command_does_not_panic() {
        // The default (no sink installed) transport is a `tracing::info!` no-op.
        emit_command();
    }
}
