//! Cross-platform primitives and transport helpers.
//!
//! The system clock, Tokio task spawner, HTTP/MCP connectors, mobile Linux
//! adapters, and shared configuration are available to all consumers. The
//! `desktop` feature enables the native watcher and desktop LSP transport;
//! POSIX and Windows keep their respective child-process command policies.

#![forbid(unsafe_code)]

pub mod clock;
pub mod guest_fs;
pub mod http;
pub mod llm_config;
#[cfg(feature = "desktop")]
pub mod lsp;
pub mod mcp_http;
pub mod mcp_proxy;
pub mod mcp_remote;
pub mod mcp_sse;
pub mod mcp_stdio;
pub mod mcp_ws;
pub mod mobile_linux;
pub mod proxy_auth_helper;
pub mod runtime;
#[cfg(feature = "desktop")]
pub mod watch_helper;
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

pub use http_client::{provider_transport, provider_transport_with_proxy_auth_helper};
pub use lingxi_llm_client::transport::ProxyAuthHelper;
