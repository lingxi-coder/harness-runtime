//! Stateless renderer for `/help` output. Produces a byte-locked string
//! formatted per plan M5-10 Task 0 step 2.
//!
//! Format:
//!
//! ```text
//! Commands:\n
//!   /<name padded to longest+2>  <description>\n
//!   ... visible command lines, sorted ASCII-ascending ...
//! ```
//!
//! Where `<description>` is `core_description(name)` — the real per-command
//! one-liner for every visible builtin with a claude-code analogue (cp-01),
//! falling back to the literal `"(unimplemented in v0.6.0)"` only for the few
//! LingXi-specific / internal commands without one. The 8 hidden/disabled
//! commands ([`is_palette_hidden`]) are filtered out to match claude-code's
//! `commands.filter(c => !c.isHidden && !$te(c))` help/palette filter, so the
//! default total is 1 header + 76 visible commands = 77 lines: 75 visible
//! builtin rows plus the Native bundled `/doctor` prompt. The `/commit` bundled
//! prompt is omitted while `tengu_cinder_relay` is at its default-off value.

use crate::builtin_support::names::{
    core_description, is_command_env_disabled, is_palette_hidden, BUILTIN_COMMAND_NAMES,
};

/// Render the locked `/help` output as a single `String`.
///
/// Hidden / disabled commands (per [`is_palette_hidden`]) are omitted, exactly
/// as claude-code's `/help` and slash palette drop any command where
/// `isHidden || isEnabled()===off`.
///
/// The output is **byte-locked** — drift indicates the canonical surface
/// changed and must be re-locked against the parity fixture
/// `parity_help_screen.txt`.
#[must_use]
pub fn render_help_screen() -> String {
    let visible = visible_commands();
    let col1_width = visible
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0)
        + 2;

    // Capacity hint: header + visible lines.
    let mut out = String::with_capacity(10 + visible.len() * (col1_width + 40));
    out.push_str("Commands:\n");

    for (name, description) in visible {
        // Column 1: `/<name>` left-padded to col1_width characters.
        out.push_str("  /");
        out.push_str(name);
        let prefix_used = 1 + name.len(); // `/` + name
        if prefix_used < col1_width {
            for _ in prefix_used..col1_width {
                out.push(' ');
            }
        }
        out.push_str("  ");

        // Column 2: the real per-command description (cp-01). `core_description`
        // now covers every visible builtin with a claude-code analogue and
        // returns the `(unimplemented in v0.6.0)` placeholder only for the few
        // LingXi-specific / internal commands without one — so `/help` and the
        // slash palette stay in lock-step.
        out.push_str(description);
        out.push('\n');
    }

    out
}

fn visible_commands() -> Vec<(&'static str, &'static str)> {
    let mut commands = BUILTIN_COMMAND_NAMES
        .iter()
        .copied()
        .filter(|name| !is_palette_hidden(name) && !is_command_env_disabled(name))
        .map(|name| (name, core_description(name)))
        .collect::<Vec<_>>();

    // Native `Mo()` registers `/doctor` as an enabled bundled prompt unless
    // its explicit environment gate is set. `/commit` is a separate bundled
    // prompt whose `bvr()` gate defaults off.
    if !is_command_env_disabled("doctor") {
        commands.push((
            "doctor",
            "Health-check your setup and fix issues: unused extensions, duplicated or bloated memory files, slow hooks, permissions",
        ));
    }
    if telemetry::flag_bool("tengu_cinder_relay", false) {
        commands.push(("commit", "Create a git commit"));
    }

    commands.sort_by_key(|(name, _)| *name);
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_starts_with_locked_header() {
        let s = render_help_screen();
        assert!(s.starts_with("Commands:\n"));
    }

    #[test]
    fn output_has_exactly_77_lines_at_default_native_factory_gates() {
        // 1 header + 76 visible commands = 77 lines. This is 83 builtins minus
        // 8 hidden/default-disabled rows, plus bundled `/doctor`; `/commit` is
        // absent while its GrowthBook gate is at the Native default false.
        // Serialize with the env-gate mutators (names::ENV_LOCK) so a concurrent
        // `DISABLE_*_COMMAND` mutation can't transiently drop a counted command.
        let _g = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        telemetry::test_clear_flag("tengu_cinder_relay");
        std::env::remove_var("DISABLE_DOCTOR_COMMAND");
        let s = render_help_screen();
        assert!(
            s.lines()
                .any(|line| line.starts_with("  /team-onboarding ")),
            "the enabled Native team-onboarding prompt must remain visible in /help"
        );
        let n = s.matches('\n').count();
        assert_eq!(
            n, 77,
            "expected 77 lines (1 header + 76 visible commands), got {n}"
        );
    }

    #[test]
    fn hidden_and_disabled_commands_are_omitted() {
        use crate::builtin_support::names::{
            CORRECT_BY_DESIGN_STUBS, HIDDEN_PALETTE_COMMANDS, STATICALLY_DISABLED_COMMANDS,
            USAGE_CREDITS_BNR_GATED,
        };
        let s = render_help_screen();
        for name in HIDDEN_PALETTE_COMMANDS
            .iter()
            .copied()
            .chain(USAGE_CREDITS_BNR_GATED.iter().copied())
            .chain(STATICALLY_DISABLED_COMMANDS.iter().copied())
            .chain(CORRECT_BY_DESIGN_STUBS.iter().map(|(n, _)| *n))
        {
            let needle = format!("  /{name} ");
            assert!(
                !s.contains(&needle),
                "/{name} is hidden/disabled and must not appear in /help"
            );
        }
    }

    #[test]
    fn visible_host_bound_commands_still_appear() {
        // claude-code SHOWS these (no isHidden/isEnabled gate), so /help must too.
        let s = render_help_screen();
        for name in [
            "btw",
            // x402 was removed from the command set (0 hits in the 2.1.216+ oracle).
            "reload-plugins",
            "install-slack-app",
            "mobile",
            "desktop",
        ] {
            assert!(
                s.contains(&format!("  /{name} ")),
                "/{name} is visible in claude-code and must appear in /help"
            );
        }
    }

    #[test]
    fn first_command_line_is_add_dir_with_real_description() {
        let s = render_help_screen();
        let line2 = s.lines().nth(1).unwrap();
        // Col-1 width = longest_name (rate-limit-options = 18) + 2 = 20.
        // "/add-dir" (8 chars: `/`+`add-dir`) gets 12 spaces of right-padding
        // to reach col-1=20, then 2 spaces of separator before the description.
        // (cp-01) add-dir now carries its real claude-code description.
        let expected = format!(
            "  /add-dir{}  Add a new working directory",
            " ".repeat(20 - 8)
        );
        assert_eq!(line2, expected);
    }

    #[test]
    fn agents_line_uses_core_description() {
        let s = render_help_screen();
        let line = s.lines().find(|l| l.starts_with("  /agents ")).unwrap();
        // "/agents" = 7 chars; pad 13 spaces to col1=20; then 2 separator
        // spaces; then the (M4 cc2.1.198) removed-wizard description, verbatim
        // from the binary's `name:"agents"` command object.
        let expected = format!(
            "  /agents{}  {}",
            " ".repeat(20 - 7),
            core_description("agents"),
        );
        assert_eq!(line, expected);
    }

    #[test]
    fn every_visible_command_appears_once() {
        // Serialize with the env-gate mutators (names::ENV_LOCK) so a concurrent
        // `DISABLE_LOGIN_COMMAND` (etc.) mutation can't drop a command mid-render.
        let _g = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        telemetry::test_clear_flag("tengu_cinder_relay");
        std::env::remove_var("DISABLE_DOCTOR_COMMAND");
        let s = render_help_screen();
        for (name, _) in visible_commands() {
            // Match the exact line-start pattern `  /<name> ` (with trailing
            // space) to avoid prefix collisions like `/commit` matching
            // inside `/git-commit`.
            let needle = format!("  /{name} ");
            let count = s.matches(&needle).count();
            assert_eq!(count, 1, "/{name} should appear exactly once, got {count}");
        }
    }

    #[test]
    fn output_ends_with_newline() {
        let s = render_help_screen();
        assert!(s.ends_with('\n'));
    }
}
