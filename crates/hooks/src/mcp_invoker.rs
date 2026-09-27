use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Name-addressed MCP seam used by `mcp_tool` hooks.
///
/// The hooks crate deliberately depends on an ALREADY-CONNECTED registry owned
/// by the composition root rather than discovering or dialing MCP servers
/// itself. Callers should return `NotConnected` when the named server is not
/// currently live; the hook executor treats that as a non-blocking hook error.
#[async_trait]
pub trait HookMcpInvoker: Send + Sync {
    async fn invoke(&self, request: HookMcpInvocation) -> HookMcpInvocationResult;
}

/// One `mcp_tool` call request built by the hook executor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookMcpInvocation {
    pub server: String,
    pub tool: String,
    pub input: HashMap<String, Value>,
    pub timeout: Duration,
}

/// Outcome returned by the composition root's MCP registry adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HookMcpInvocationResult {
    Success {
        text_content: Vec<String>,
    },
    Error {
        text_content: Vec<String>,
        message: String,
    },
    NotConnected {
        message: String,
    },
    Timeout {
        text_content: Vec<String>,
    },
}
