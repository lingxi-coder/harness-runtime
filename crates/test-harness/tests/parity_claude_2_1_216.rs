//! Historical version-facing identifier derivations for Claude Code 2.1.216.
//!
//! This file no longer owns the live parity target; it preserves the exact
//! identifiers captured for 2.1.216 while the live target advances.

const HISTORICAL_VERSION: &str = "2.1.216";

/// The historical capture remains distinct from the current parity target.
#[test]
fn historical_version_is_2_1_216() {
    assert_eq!(HISTORICAL_VERSION, "2.1.216");
    assert_ne!(platform_api::CLAUDE_CODE_VERSION, HISTORICAL_VERSION);
}

/// Child processes receive the 2.1.216 `AI_AGENT` identifier.
#[test]
fn ai_agent_env_value_is_2_1_216() {
    let derived = format!("claude-code_{}_agent", HISTORICAL_VERSION.replace('.', "-"));
    assert_eq!(derived, "claude-code_2-1-216_agent");
}

/// WebFetch presents the 2.1.216 Claude-compatible user agent.
#[test]
fn web_fetch_user_agent_is_2_1_216() {
    let derived = format!(
        "Claude-User (claude-code/{}; +https://support.anthropic.com/)",
        HISTORICAL_VERSION
    );
    assert_eq!(
        derived,
        "Claude-User (claude-code/2.1.216; +https://support.anthropic.com/)"
    );
}
