//! `/cd` — byte-exact model-visible strings for the "move this session to a
//! new working directory" command (claude-code 2.1.207 `local-jsx` `name:"cd"`).
//!
//! The command itself is interactive (a confirm prompt, then a
//! [`crate::builtin_support`]-registered `SessionCwd` swap driven by the TUI +
//! the CLI `run_permission_action` effect channel). This module is the single
//! source of the two byte-faithful strings both surfaces render — the confirm
//! prompt and the post-move result message — so the TUI confirm view
//! (`tui::bottom_pane::cd_confirm_view`) and the CLI swap effect
//! (`apps/cli/src/mode.rs`) can never drift.
//!
//! Oracle (2.1.207, canonical binary):
//! - confirm prompt @ byte 198796864 (`? This moves the session's working …`).
//! - result template @ the `tengu_cd_command` module: `The session's working
//!   directory has changed to ${s} (${t==="cd_command"?"via /cd":"by the
//!   user"}). The environment block at the start of this conversation still
//!   names the ` + `previous directory — that information is stale. All
//!   tool calls and ` + `relative paths now resolve from ${s}.` — for the `/cd`
//!   slash path `t === "cd_command"`, so the parenthetical is `via /cd`.

/// The confirm prompt shown before the working directory is moved. Byte-exact
/// to claude-code 2.1.207's `/cd` safety-check message (142 bytes, including
/// the leading `? ` and the ASCII apostrophe).
pub const CONFIRM_PROMPT: &str = "? This moves the session's working directory and write access there, and loads project configuration (CLAUDE.md, settings) from that location.";

/// The `tengu_*` analytics event name claude-code emits after a successful
/// `/cd` move (`N("tengu_cd_command", { source: xe(t) })`).
pub const TELEMETRY_EVENT: &str = "tengu_cd_command";

/// The `source` property value for a move triggered by the `/cd` slash command
/// (claude-code's `t = "cd_command"`; the analytics layer hashes it before the
/// wire, so this is the pre-hash semantic source, not the wire bytes).
pub const TELEMETRY_SOURCE_CD_COMMAND: &str = "cd_command";

/// The post-move result message for the `/cd` slash path, byte-exact to
/// claude-code 2.1.207. `new_cwd` is interpolated at BOTH `${s}` sites (the
/// binary uses the same formatted new path in each). The message deliberately
/// does NOT name the previous directory — it only notes that the frozen
/// `<env>` block still references it and is now stale.
#[must_use]
pub fn result_message(new_cwd: &str) -> String {
    // Fragment-by-fragment to mirror the binary's template/concat boundaries
    // exactly, keeping every inter-word space unambiguous (no `\`-continuation
    // whitespace hazard).
    let mut s = String::new();
    s.push_str("The session's working directory has changed to ");
    s.push_str(new_cwd);
    s.push_str(
        " (via /cd). The environment block at the start of this conversation still names the ",
    );
    s.push_str("previous directory \u{2014} that information is stale. All tool calls and ");
    s.push_str("relative paths now resolve from ");
    s.push_str(new_cwd);
    s.push('.');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_prompt_is_byte_exact() {
        assert_eq!(
            CONFIRM_PROMPT,
            "? This moves the session's working directory and write access there, and loads project configuration (CLAUDE.md, settings) from that location."
        );
        // The oracle constant is 142 bytes (leading `? `, ASCII apostrophe).
        assert_eq!(CONFIRM_PROMPT.len(), 142);
        assert!(CONFIRM_PROMPT.starts_with("? This moves the session's"));
        assert!(CONFIRM_PROMPT.ends_with("from that location."));
    }

    #[test]
    fn telemetry_names_are_byte_exact() {
        assert_eq!(TELEMETRY_EVENT, "tengu_cd_command");
        assert_eq!(TELEMETRY_SOURCE_CD_COMMAND, "cd_command");
    }

    #[test]
    fn result_message_is_byte_exact() {
        let got = result_message("/tmp/new");
        // Single-line expected literal (no `\`-continuations) so every space is
        // unambiguous. The em-dash is U+2014.
        let expected = "The session's working directory has changed to /tmp/new (via /cd). The environment block at the start of this conversation still names the previous directory \u{2014} that information is stale. All tool calls and relative paths now resolve from /tmp/new.";
        assert_eq!(got, expected);
    }

    #[test]
    fn result_message_interpolates_path_at_both_sites() {
        let got = result_message("/home/me/project");
        assert_eq!(got.matches("/home/me/project").count(), 2);
        assert!(got.starts_with(
            "The session's working directory has changed to /home/me/project (via /cd)."
        ));
        assert!(got.ends_with("relative paths now resolve from /home/me/project."));
        // The em-dash is U+2014, not a hyphen.
        assert!(got.contains("previous directory \u{2014} that information"));
    }
}
