//! Production Windows platform crate.
//!
//! Implements `lingxi-traits` interfaces using Windows-friendly tokio
//! primitives. Most trait surface is identical to the POSIX crate; only
//! `flock`, `watch`, `Sandbox`, and `Swarm` diverge. Platform-specific
//! primitives (Job Objects, `ReadDirectoryChangesW`, Credential Vault) are
//! TODO(M2-followup) — current stubs use cross-platform fallbacks.
//!
//! This crate compiles on any host (uses `cfg(target_os = "windows")` for
//! Windows-only fast paths) so cross-compile CI gates work without a
//! Windows runner.

// Keep unsafe denied crate-wide; narrowly scoped Windows OS integration modules
// opt in explicitly around the APIs whose contracts they uphold.
#![deny(unsafe_code)]

pub mod bridge;
pub mod clock;
pub mod fs;
pub mod http;
pub mod lsp;
pub mod mcp;
pub mod process;
pub mod runtime;
pub mod sandbox;
pub mod secure_storage;
#[cfg(windows)]
pub mod stdio_cancel;
pub mod swarm;
#[cfg(windows)]
pub mod system_paths;
pub(crate) mod watch_helper;
pub mod worktree;

pub use bridge::WindowsBridgeTransport;
pub use clock::WindowsClock;
pub use fs::WindowsFileSystem;
pub use http::WindowsHttp;
pub use lsp::WindowsLspTransport;
pub use mcp::WindowsMcpTransport;
// Convenience re-exports at the crate root so callers can write
// `platform_windows::{connect_ws, spawn_stdio}` directly — mirrors
// the layout exposed by `lingxi-platform-posix`.
pub use mcp::{connect_ws, spawn_stdio, McpTransportError};
pub use process::WindowsProcess;
pub use runtime::WindowsRuntime;
pub use sandbox::WindowsSandbox;
pub use secure_storage::{
    plaintext_secure_storage, secure_storage_for_platform, secure_storage_for_policy,
    PlainTextSecureStorage, WindowsCredentialVaultStorage,
};
pub use swarm::WindowsSwarmBackend;
pub use worktree::WindowsWorktreeManager;
