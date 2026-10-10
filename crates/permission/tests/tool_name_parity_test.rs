//! Cross-crate parity test: every tool name from the per-category tool crates
//! must resolve via `permission::tool_default`. Uses `lingxi-tools`
//! as a dev-dep (no build cycle — `lingxi-tools → lingxi-permission` is the
//! real edge; this test-only dep reverses for verification).
//!
//! Source for the constants: `grep -n 'pub const TOOL_NAME\|pub const.*_TOOL_NAME'
//! lingxi-code/crates/tools/src/builtin/*.rs`.

use permission::tool_default;
use permission::PromptDefault;

#[test]
fn bash_constant_resolves_deny() {
    assert_eq!(
        tool_default(tool_shell::bash::TOOL_NAME),
        PromptDefault::DenyByDefault
    );
}

#[test]
fn read_constant_resolves_allow() {
    assert_eq!(
        tool_default(tool_file::read::TOOL_NAME),
        PromptDefault::AllowByDefault
    );
}

#[test]
fn write_constant_resolves_deny() {
    assert_eq!(
        tool_default(tool_file::write::TOOL_NAME),
        PromptDefault::DenyByDefault
    );
}

#[test]
fn edit_constant_resolves_deny() {
    assert_eq!(
        tool_default(tool_file::edit::TOOL_NAME),
        PromptDefault::DenyByDefault
    );
}

#[test]
fn agent_constant_resolves_allow() {
    assert_eq!(
        tool_default(tool_agent::agent::AGENT_TOOL_NAME),
        PromptDefault::AllowByDefault
    );
    assert_eq!(tool_default("Task"), PromptDefault::DenyByDefault);
}

#[test]
fn web_fetch_constant_resolves_deny() {
    assert_eq!(
        tool_default(tool_web::web_fetch::TOOL_NAME),
        PromptDefault::DenyByDefault
    );
}

#[test]
fn web_search_constant_resolves_deny() {
    assert_eq!(
        tool_default(tool_web::web_search::TOOL_NAME),
        PromptDefault::DenyByDefault
    );
}

#[test]
fn mcp_constant_resolves_deny() {
    assert_eq!(
        tool_default(tool_mcp::mcp_tool::MCP_TOOL_NAME),
        PromptDefault::DenyByDefault
    );
}
