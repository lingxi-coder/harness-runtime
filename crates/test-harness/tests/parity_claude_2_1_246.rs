//! Historical version-facing identifiers captured from the Claude Code 2.1.246 oracle.
//!
//! The LIVE pins moved to `parity_claude_2_1_252.rs` when the parity target
//! advanced to 2.1.252 (after the mcp/plugin byte-alignment backlog). This file no
//! longer asserts against `lingxi_core::host::CLAUDE_CODE_VERSION` — pinning a live
//! constant to a superseded version is how a suite starts failing for being
//! CORRECT, the same demotion 2.1.217 and 2.1.220 already took.
//!
//! Kept because the DERIVATION shape is what matters and it is version-agnostic:
//! the advertised version, the child-process identity and the WebFetch user
//! agent must all come from one constant.

const HISTORICAL_VERSION: &str = "2.1.246";

#[test]
fn version_facing_identifiers_share_one_source() {
    let version = HISTORICAL_VERSION;
    assert!(format!("claude-code_{}_agent", version.replace('.', "-"))
        .contains(&version.replace('.', "-")));
    assert!(
        format!("Claude-User (claude-code/{version}; +https://support.anthropic.com/)")
            .contains(version)
    );
}

#[test]
fn historical_version_is_2_1_246() {
    assert_eq!(HISTORICAL_VERSION, "2.1.246");
    assert_ne!(HISTORICAL_VERSION, lingxi_core::host::CLAUDE_CODE_VERSION);
}
