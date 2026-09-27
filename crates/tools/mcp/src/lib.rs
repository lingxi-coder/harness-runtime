//! MCP tools: MCPTool, McpAuth, ListMcpResources, ReadMcpResource,
//! ReadMcpResourceDir, WaitForMcpServers. Extracted in M8-P7. Desktop-only.
//! Impl module is `mcp_tool` (not `mcp`) to avoid colliding with the extern
//! `mcp` crate dep.
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::match_wildcard_for_single_variants,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::format_collect,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::manual_let_else
)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 14 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod auto_background;
pub mod large_output;
pub mod mcp_tool;
pub mod read_mcp_resource_dir;
pub mod transform_result;
pub mod wait_for_mcp_servers;
pub use large_output::{
    process_mcp_result, process_mcp_result_with_exact_count, ExactCountOutcome,
};
pub use mcp_tool::{
    build_registered_mcp_tools, configured_permission_ceiling, ListMcpResourcesTool, MCPTool,
    McpAuthTool, ReadMcpResourceTool,
};
pub use read_mcp_resource_dir::ReadMcpResourceDirTool;
pub use transform_result::transform_result_content;
pub use wait_for_mcp_servers::WaitForMcpServersTool;

/// Register the MCP tools (call, auth, wait-for-servers) against `reg`.
///
/// The three MCP-RESOURCE tools (`ListMcpResourcesTool`, `ReadMcpResourceTool`,
/// `ReadMcpResourceDirTool`) are deliberately NOT registered here. 2.1.238's
/// `getTools` (`iJ`) strips all three out of the base tool list by name —
///
/// ```text
/// let r=new Set([K8.name,Z8.name,Yme.name,Ky]),n=ZY().filter((c)=>!r.has(c.name))
/// ```
///
/// (`cc-238.js @230759264`) — and MCP discovery pushes them into the MCP tool
/// partition only once a CONNECTED server declares `capabilities.resources`:
///
/// ```text
/// if(n.some((l)=>l.type==="connected"&&!!l.capabilities?.resources)){
///   if(![K8,Z8].some((c)=>o.some((u)=>il(u,c.name))))o.push(K8,Z8,Yme)}
/// ```
///
/// (`cc-238.js @225994181`, same shape at `@243875044`). The port reproduces
/// that push in [`build_registered_mcp_tools`], so a session with no
/// resource-capable MCP server no longer advertises resource tools it cannot
/// use.
///
/// `WaitForMcpServers` IS registered here — it is a base-list tool in both
/// 2.1.220 and 2.1.238 — but its `is_enabled` reads
/// `McpRegistry::has_pending_servers`, so it is advertised only while a server
/// is still connecting (oracle `R8v`'s `bdl(t).length>0` leg). See
/// `wait_for_mcp_servers::WaitForMcpServersTool`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    // Generic `MCP` dispatch is kept for internal compatibility tests only.
    // Production registration uses discovered per-tool entries so an FQN
    // cannot bypass its server/tool-specific permission rule.
    reg.register_builtin(Arc::new(McpAuthTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(WaitForMcpServersTool::new(ctx)));
}

mod task_result;
