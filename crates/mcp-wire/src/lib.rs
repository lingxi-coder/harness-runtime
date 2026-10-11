//! Dependency-neutral, source-preserving MCP transport and wire contracts.
#![forbid(unsafe_code)]
pub mod export;
mod ids;
pub mod transport;
pub use ids::McpConnectionId;
pub use transport::{
    McpIconDto, McpPermissionCeiling, McpToolAnnotationsDto, McpToolDefinitionDto,
    McpToolExecutionDto, McpToolTaskSupportDto,
};
