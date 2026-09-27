//! Compile-only smoke test for the new client module surface.
//!
//! If this file fails to compile, the type contract in plan
//! `2026-05-23-m2-02b-mcp-client.md` is broken.

#[test]
fn module_surface_is_reachable() {
    // Touch each public type to force a compile-time check.
    let _: Option<mcp::McpClient> = None;
    let _: Option<mcp::ClientInfo> = None;
    let _: Option<mcp::InitializeParams> = None;
    let _: Option<mcp::McpClientError> = None;
    let _: Option<mcp::RootsListHandler> = None;
    let _: Option<mcp::ElicitationCreateHandler> = None;
}
