//! Minimal posix-style platform implementation suitable for the M1.22
//! cli-demo and host-side tests.
//!
//! Real I/O is provided for the trait pairs the demo exercises today —
//! [`FileSystem`](platform_api::FileSystem),
//! [`RuntimeSpawner`](platform_api::RuntimeSpawner), and
//! [`Clock`](platform_api::Clock). Every other trait is stubbed with the
//! "unsupported / unavailable" branch of its error enum so engine code that
//! reaches them during the demo fails loudly rather than silently no-oping.
//!
//! The hardened POSIX platform (sandbox-exec / Linux namespaces, real LSP
//! and MCP transports, `git worktree` shell-out, OS keychain) lands in
//! `platforms/posix` during Plan 17.

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
