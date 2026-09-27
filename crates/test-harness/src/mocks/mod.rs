//! Mock implementations of platform traits for deterministic engine tests.

pub mod mock_clock;
pub mod mock_http;
pub mod mock_lsp_server;
pub mod mock_mcp;
pub mod mock_runtime;

pub use mock_clock::MockClock;
pub use mock_http::{MockHttpTransport, ScriptedResponse};
pub use mock_mcp::MockMcpTransport;
pub use mock_runtime::MockRuntimeSpawner;
