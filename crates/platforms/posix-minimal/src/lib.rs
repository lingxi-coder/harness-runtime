//! Portable primitives and unavailable capability implementations shared by
//! demo, test, and mobile composition roots.
//!
//! [`PosixFileSystem`] performs real filesystem I/O and advisory locking; its
//! workspace root supplies an `is_within_workspace` predicate, not an I/O
//! confinement boundary. Its watch stream is empty. [`PosixClock`] and
//! [`PosixRuntime`] are portable local implementations; keeping these small
//! primitives here avoids pulling the larger platform-common transport graph
//! into minimal-only consumers. [`PosixHttp`] uses the real HTTP client.
//!
//! Process execution, sandbox preparation, LSP, MCP, bridge, worktree, swarm,
//! and credential persistence are unavailable or non-persisting test/demo
//! implementations. Mobile composition chooses its real Linux runtime and
//! native credential services separately when supplied; these placeholders do
//! not provide those capabilities. Desktop composition uses `platform-posix`
//! or `platform-windows` for its platform-specific implementations.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod clock;
pub mod fs;
pub mod http;
pub mod lsp;
pub mod mcp;
pub mod notification;
pub mod process;
pub mod runtime;
pub mod sandbox;
pub mod secure_storage;
pub mod swarm;
pub mod worktree;

pub use bridge::PosixBridge;
pub use clock::PosixClock;
pub use fs::PosixFileSystem;
pub use http::PosixHttp;
pub use lsp::PosixLsp;
pub use mcp::PosixMcp;
pub use process::PosixProcess;
pub use runtime::PosixRuntime;
pub use sandbox::PosixSandbox;
pub use secure_storage::PlainTextSecureStorage;
pub use swarm::PosixSwarm;
pub use worktree::PosixWorktree;
