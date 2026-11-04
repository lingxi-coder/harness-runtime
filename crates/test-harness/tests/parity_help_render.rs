//! Parity: `/help` byte-locked rendering. Compares the renderer output to
//! the golden `parity_help_screen.txt`.
//!
//! M5-10 Task 10.

use command_api::builtin_support::help_render::render_help_screen;

const GOLDEN: &str = include_str!("../src/parity/fixtures/parity_help_screen.txt");

// Project only the documented product differences from the immutable Native
// screen: the two account commands and product-facing descriptions.
fn product_branded_golden() -> String {
    const NATIVE_AGENTS_DESCRIPTION: &str =
        "(removed) Ask Claude to create/manage subagents, or edit .claude/agents/";
    assert_eq!(
        GOLDEN.matches(NATIVE_AGENTS_DESCRIPTION).count(),
        1,
        "the source baseline contains one versioned /agents description"
    );
    const NATIVE_TEAM_DESCRIPTION: &str =
        "Help teammates ramp on Claude Code with a guide from your usage";
    assert_eq!(GOLDEN.matches(NATIVE_TEAM_DESCRIPTION).count(), 1);
    let mut projected = GOLDEN
        .replace(
            NATIVE_AGENTS_DESCRIPTION,
            &format!(
                "(removed) Ask {} to create/manage subagents, or edit {}/agents/",
                branding::PRODUCT_NAME,
                branding::DOT_DIR
            ),
        )
        .replace(
            NATIVE_TEAM_DESCRIPTION,
            &format!(
                "Help teammates ramp on {} with a guide from your usage",
                branding::PRODUCT_NAME
            ),
        );
    for (name, source_row) in [
        (
            "login",
            "  /login                Sign in with your Anthropic account",
        ),
        (
            "logout",
            "  /logout               Sign out from your Anthropic account",
        ),
    ] {
        let prefix = format!("  /{name} ");
        let rows: Vec<_> = projected
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .collect();
        assert_eq!(
            rows,
            [source_row],
            "the Native /{name} row must match exactly"
        );
        let row = format!("{source_row}\n");
        projected = projected.replacen(&row, "", 1);
    }
    projected
}

#[test]
fn render_help_screen_matches_native_golden_with_product_projection() {
    let actual = render_help_screen();
    assert_eq!(
        actual,
        product_branded_golden(),
        "/help output drifted outside the product branding and account-command projection"
    );
}

#[test]
fn golden_starts_with_locked_header() {
    assert!(GOLDEN.starts_with("Commands:\n"));
}

#[test]
fn golden_has_77_lines_and_native_team_onboarding() {
    // 1 header + 76 visible commands = 77 newline-terminated lines.
    // The fixed 2.1.295 catalog has 83 builtins, 8 hidden/disabled entries,
    // and the separately registered bundled `/doctor` row.
    assert!(
        GOLDEN
            .lines()
            .any(|line| line.starts_with("  /team-onboarding ")),
        "the source-visible Native team-onboarding row must be in the golden"
    );
    assert_eq!(GOLDEN.matches('\n').count(), 77);
}

#[test]
fn product_projection_removes_only_the_two_account_rows() {
    let projected = product_branded_golden();
    assert_eq!(projected.lines().count(), 75);
    for name in ["login", "logout"] {
        assert!(!projected
            .lines()
            .any(|line| line.starts_with(&format!("  /{name} "))));
    }
    assert!(projected
        .lines()
        .any(|line| line.starts_with("  /team-onboarding ")));
}

#[test]
fn golden_omits_hidden_and_disabled_commands() {
    use command_api::builtin_support::names::{
        CORRECT_BY_DESIGN_STUBS, HIDDEN_PALETTE_COMMANDS, STATICALLY_DISABLED_COMMANDS,
        USAGE_CREDITS_BNR_GATED,
    };
    for name in HIDDEN_PALETTE_COMMANDS
        .iter()
        .copied()
        .chain(USAGE_CREDITS_BNR_GATED.iter().copied())
        .chain(STATICALLY_DISABLED_COMMANDS.iter().copied())
        .chain(CORRECT_BY_DESIGN_STUBS.iter().map(|(n, _)| *n))
    {
        assert!(
            !GOLDEN.contains(&format!("  /{name} ")),
            "/{name} is hidden/disabled and must not appear in the /help golden"
        );
    }
}
