//! Locks the wire format of `McpError::Timeout` for downstream tests in
//! plan M2-07. Format MUST be:
//!
//!     MCP server "<name>" tool "<tool>" timed out after <N>s
//!
//! Do not change this string without updating M2-02b §"Critical 1:1 fidelity
//! items" AND M2-07 integration tests in lockstep.

use lingxi_core::host::McpError;

#[test]
fn timeout_display_format_matches_claude_code() {
    let err = McpError::Timeout {
        server: "filesystem".to_string(),
        tool: "read_file".to_string(),
        secs: 60,
    };
    assert_eq!(
        err.to_string(),
        r#"MCP server "filesystem" tool "read_file" timed out after 60s"#,
    );
}
