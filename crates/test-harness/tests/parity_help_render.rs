//! Parity: `/help` byte-locked rendering. Compares the renderer output to
//! the golden `parity_help_screen.txt`.
//!
//! M5-10 Task 10.

use command_api::builtin_support::help_render::render_help_screen;

const GOLDEN: &str = include_str!("../src/parity/fixtures/parity_help_screen.txt");

// 2.1.295 HSo in acceptance/rea-evaluation-20261011/native-chunks/src_194447854.js uses Claude/.claude; `/agents` is product-branded here.
fn product_branded_golden() -> String {
    const NATIVE_AGENTS_DESCRIPTION: &str =
        "(removed) Ask Claude to create/manage subagents, or edit .claude/agents/";
    assert_eq!(
        GOLDEN.matches(NATIVE_AGENTS_DESCRIPTION).count(),
        1,
        "the source baseline contains one versioned /agents description"
    );
    GOLDEN.replace(
        NATIVE_AGENTS_DESCRIPTION,
        &format!(
            "(removed) Ask {} to create/manage subagents, or edit {}/agents/",
            branding::PRODUCT_NAME,
            branding::DOT_DIR
        ),
    )
}

#[test]
fn render_help_screen_matches_native_golden_with_product_agents_identity() {
    let actual = render_help_screen();
    assert_eq!(
        actual,
        product_branded_golden(),
        "/help output drifted outside the approved /agents branding projection"
    );
}

#[test]
fn golden_starts_with_locked_header() {
    assert!(GOLDEN.starts_with("Commands:\n"));
}

#[test]
fn golden_has_80_lines() {
    // 1 header + 79 VISIBLE commands = 80 lines (each `\n`-terminated).
    // The hidden/disabled commands (is_palette_hidden) are filtered out,
    // matching claude-code's `commands.filter(c => !c.isHidden && !$te(c))`.
    // (87 builtins − 8 hidden = 79, no DISABLE_*_COMMAND env set.)
    // SLASH-06 moved `bug` into the visible set and dropped the phantom
    // `share`; SLASH-14 then moved `/version` OUT of it — both of its oracle
    // objects @296268759 carry `isEnabled:()=>!1`, so claude-code's own
    // `!$te(c)` filter never lists it. The current 2.1.252 audit removes the
    // stale internal command objects while retaining the ungated `/powerup`
    // object and its lesson handler.
    // cc2.1.269 brought `/output-style` back as a visible `type:"local"`
    // command, which is the whole of the 79 → 80 move.
    assert_eq!(GOLDEN.matches('\n').count(), 80);
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
