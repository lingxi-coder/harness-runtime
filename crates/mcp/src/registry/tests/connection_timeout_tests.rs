//! MCPLIFE.4 — connect deadline parity with claude-code
//! `getConnectionTimeoutMs()` (`parseInt(MCP_TIMEOUT) || 30000`).
use super::mcp_connection_timeout;
use std::time::Duration;

// The only test in this crate that mutates MCP_TIMEOUT; the connect-path
// tests don't assert the deadline, so the shared-env mutation is harmless.
#[test]
fn timeout_defaults_to_30s_and_honors_positive_env() {
    std::env::remove_var("MCP_TIMEOUT");
    assert_eq!(mcp_connection_timeout(), Duration::from_secs(30));
    std::env::set_var("MCP_TIMEOUT", "5000");
    assert_eq!(mcp_connection_timeout(), Duration::from_millis(5000));
    // parseInt(..) || 30000 — zero / non-numeric / empty fall back to 30s.
    std::env::set_var("MCP_TIMEOUT", "0");
    assert_eq!(mcp_connection_timeout(), Duration::from_secs(30));
    std::env::set_var("MCP_TIMEOUT", "not-a-number");
    assert_eq!(mcp_connection_timeout(), Duration::from_secs(30));
    std::env::remove_var("MCP_TIMEOUT");
}
