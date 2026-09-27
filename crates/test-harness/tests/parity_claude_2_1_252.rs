//! What the Claude Code 2.1.252 release said, captured from that binary.
//!
//! The live version-facing identifier pins moved to
//! `parity_version_identifiers.rs` when the constant was raised to 2.1.267:
//! they were never about 2.1.252, they were about the derivation, and a version
//! in the file name made them look like a release capture. What stays here is
//! one — the `claude agents` help text as 2.1.252 printed it.

const AGENTS_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_252_agents_help.txt");

#[test]
fn agents_help_fixture_pins_restricted_dispatch_surface() {
    assert!(AGENTS_HELP.starts_with("Usage: claude agents [options]\n"));
    assert!(AGENTS_HELP.contains(
        "  --restricted                          Start dispatched sessions in restricted\n                                        mode\n"
    ));
}
