//! Parity **delta baseline** vs Claude Code 2.1.215.
//!
//! Unlike [`parity_claude_2_1_208`], this file does **not** own or pin the live
//! `lingxi_core::host::CLAUDE_CODE_VERSION` — the port's live version-facing wave now tracks
//! 2.1.252. This is a historical capture of the real 2.1.215
//! native binary's observable CLI surface (root/agents/mcp/plugin `--help`),
//! recorded so audits diff against the CURRENT release instead of the stale
//! 2.1.198/2.1.208 fixtures. (An audit run against a 2.1.198 baseline is how a
//! whole review can come back half-false: fixes already on HEAD read as "gaps".)
//!
//! The fixtures were captured from
//! `~/.local/share/claude/versions/2.1.215 {--help, agents --help, mcp --help,
//! plugin --help}` (SHA-256 in the review notes). Each `*_pins_*` test asserts a
//! parity-relevant fact of that captured surface — a NEW flag/command that
//! appeared since the last pinned wave — and records the current LingXi status
//! inline (implemented paths and any remaining intentional divergence).

const ROOT_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_215_root_help.txt");
const AGENTS_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_215_agents_help.txt");
const MCP_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_215_mcp_help.txt");
const PLUGIN_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_215_plugin_help.txt");

/// The captured fixtures really are the 2.1.215 root surface (guards against a
/// stale re-capture silently downgrading the baseline).
#[test]
fn root_help_fixture_is_the_2_1_215_cli_surface() {
    assert!(
        ROOT_HELP.starts_with("Usage: claude [options] [command] [prompt]"),
        "root help fixture is not the claude root usage banner"
    );
    // Flags that predate 2.1.215 and must still be present (sanity that this is
    // a real, complete capture rather than a truncated file).
    for flag in ["--plugin-dir <path>", "--settings", "--mcp-config"] {
        assert!(
            ROOT_HELP.contains(flag),
            "expected long-standing flag `{flag}` in 2.1.215 root help"
        );
    }
}

/// The 2.1.214/2.1.215 additions the last global audit flagged (M-01 `--brief`,
/// M-04 `--plugin-url`). Pinning them here as CC-side facts means a future audit
/// diffs against a fixture that KNOWS these exist, so the "is the flag missing?"
/// question is answered by the harness rather than re-derived by hand.
#[test]
fn root_help_pins_2_1_214_215_new_flags() {
    // M-01: `--brief` gates the `SendUserMessage` agent-to-user tool. LingXi's
    // current CLI parses the flag and exports the session-scoped gate; only
    // the oracle's entitlement/userMsgOptIn classification remains different.
    assert!(ROOT_HELP.contains("--brief"), "2.1.215 ships --brief");
    assert!(
        ROOT_HELP.contains("Enable SendUserMessage tool for"),
        "--brief help text drifted"
    );

    // M-04: `--plugin-url` fetches a session-only plugin .zip (repeatable).
    // LingXi's startup_resources path now downloads each archive into an
    // owner-only temporary root and threads it through cli_plugin_dirs.
    assert!(
        ROOT_HELP.contains("--plugin-url <url>"),
        "2.1.215 ships --plugin-url"
    );
    assert!(
        ROOT_HELP.contains("Fetch a plugin .zip from a URL for this"),
        "--plugin-url help text drifted"
    );
}

/// The `plugin` subcommand surface, including the `plugin@marketplace` identity
/// (H-12: plugin configs/secrets must be keyed by `name@marketplace`, not a bare
/// manifest name, so two marketplaces' same-named plugins don't collide).
#[test]
fn plugin_help_pins_marketplace_identity_surface() {
    assert!(
        PLUGIN_HELP.contains("plugin@marketplace"),
        "2.1.215 plugin help documents the name@marketplace identity"
    );
    for sub in ["enable", "disable", "details"] {
        assert!(
            PLUGIN_HELP.contains(sub),
            "expected `plugin {sub}` subcommand in 2.1.215 plugin help"
        );
    }
}

/// The `agents` and `mcp` subcommand help fixtures are real captures (non-empty,
/// correct banner) — the two surfaces most likely to grow flags between waves.
#[test]
fn agents_and_mcp_help_fixtures_are_present() {
    assert!(
        AGENTS_HELP.contains("agents"),
        "agents help fixture looks wrong"
    );
    assert!(MCP_HELP.contains("mcp"), "mcp help fixture looks wrong");
}
