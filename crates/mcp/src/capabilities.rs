//! Re-export of capability/tool/resource DTOs for ergonomic use from
//! engine code that depends on `lingxi-mcp` rather than `lingxi-traits`.

pub use lingxi_core::host::{McpPromptDto, McpResourceDto, McpToolDto, ServerCapabilitiesDto};
