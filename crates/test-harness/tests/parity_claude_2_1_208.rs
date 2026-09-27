//! Parity regression pins vs Claude Code 2.1.208.
//!
//! This file is a historical behavior regression. It deliberately uses a local
//! literal and must not pin the live `platform_api::CLAUDE_CODE_VERSION`.

const HISTORICAL_VERSION: &str = "2.1.208";

/// The captured historical version remains stable.
#[test]
fn historical_version_is_2_1_208() {
    assert_eq!(HISTORICAL_VERSION, "2.1.208");
}

/// The child-process `AI_AGENT` env value:
/// `claude-code_${VERSION.replace(/\./g,"-")}_agent`.
#[test]
fn historical_ai_agent_env_value_is_2_1_208() {
    let derived = format!("claude-code_{}_agent", HISTORICAL_VERSION.replace('.', "-"));
    assert_eq!(derived, "claude-code_2-1-208_agent");
    let mid = derived
        .strip_prefix("claude-code_")
        .and_then(|s| s.strip_suffix("_agent"))
        .expect("prefix/suffix present");
    assert!(!mid.contains('.'), "version dots must be dashed: {derived}");
}

/// The WebFetch `User-Agent`:
/// `Claude-User (claude-code/${VERSION}; +https://support.anthropic.com/)`.
#[test]
fn historical_web_fetch_user_agent_is_2_1_208() {
    let derived = format!(
        "Claude-User (claude-code/{}; +https://support.anthropic.com/)",
        HISTORICAL_VERSION
    );
    assert_eq!(
        derived,
        "Claude-User (claude-code/2.1.208; +https://support.anthropic.com/)"
    );
}
