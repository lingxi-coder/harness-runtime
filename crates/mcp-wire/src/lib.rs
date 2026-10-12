//! Source-preserving MCP wire and transport contracts.
#![forbid(unsafe_code)]

mod ids;
pub mod transport;

pub use ids::McpConnectionId;
pub use transport::{
    McpIconDto, McpPermissionCeiling, McpToolAnnotationsDto, McpToolDefinitionDto,
    McpToolExecutionDto, McpToolTaskSupportDto,
};
