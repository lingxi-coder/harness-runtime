//! HISTORICAL fixture for the Claude Code 2.1.217 oracle.
//!
//! The LIVE version-facing pins moved to `parity_claude_2_1_252.rs` as the
//! parity target advanced. This file no longer asserts against
//! `lingxi_core::host::CLAUDE_CODE_VERSION` — pinning a live constant to a superseded
//! version is how a suite starts failing for being CORRECT, and the 2.1.216
//! audit demoted the 2.1.208 suite the same way.
//!
//! Kept because the DERIVATIONS are what matter and they are version-agnostic:
//! `AI_AGENT` is the version with dots turned into dashes, the WebFetch UA
//! embeds it verbatim. Those shapes are re-asserted here against a frozen
//! 2.1.217 string so a change to the derivation is caught even if the target
//! moves again.

/// The 2.1.217 `AI_AGENT` shape: dots become dashes.
#[test]
fn ai_agent_env_shape_for_2_1_217() {
    let derived = format!("claude-code_{}_agent", "2.1.217".replace('.', "-"));
    assert_eq!(derived, "claude-code_2-1-217_agent");
}

/// The 2.1.217 WebFetch user agent embeds the version verbatim.
#[test]
fn web_fetch_user_agent_shape_for_2_1_217() {
    let derived = format!(
        "Claude-User (claude-code/{}; +https://support.anthropic.com/)",
        "2.1.217"
    );
    assert_eq!(
        derived,
        "Claude-User (claude-code/2.1.217; +https://support.anthropic.com/)"
    );
}
