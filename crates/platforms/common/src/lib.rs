//! Cross-platform MCP transport helpers shared by `platforms/posix` and
//! `platforms/windows`.
//!
//! Stdio spawn lives per-platform because process creation differs slightly
//! between POSIX and Windows, but the WebSocket / SSE / HTTP connectors and
//! small shared utilities (stderr ring buffer, `StdioConfig`) live here to
//! avoid duplication.
//!
//! Also hosts the shared built-in Anthropic LLM config so `engine-desktop` and
//! `engine-mobile` stay in sync without duplicating the model table.

#![forbid(unsafe_code)]

pub mod guest_fs;
pub mod http;
pub mod llm_config;
pub mod mcp_http;
pub mod mcp_remote;
pub mod mcp_sse;
pub mod mcp_stdio;
pub mod mcp_ws;
pub mod mobile_linux;
pub mod worktree_create_guard;
pub mod worktree_include;

pub use guest_fs::GuestPathFileSystem;
pub use http::ReqwestHttp;
pub use llm_config::{
    apply_settings_providers, builtin_anthropic_config, parse_routing_overrides, RoutingOverrides,
};

pub use mcp_http::{connect_http, HttpConnectError};
pub use mcp_remote::RemoteMcpTransport;
pub use mcp_sse::{connect_sse, SseConnectError, SseEndpointMode, IDE_AUTH_HEADER};
pub use mobile_linux::{
    MobileLinuxProcessRunner, MobileLinuxSandbox, RootfsArchive, RootfsEntryKind,
    RootfsImmutableEntry, RootfsImmutableKind, RootfsManifest, RootfsManifestEntry,
    RootfsManifestError, RootfsPackage, RootfsStore, RootfsStoreError, RootfsVerificationIssue,
    RootfsVerificationReport,
};
pub use worktree_create_guard::reject_worktree_create_symlinks;
pub use worktree_include::copy_worktree_include_files;

pub use http_client::provider_transport;
