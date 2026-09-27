//! Trait-level contract test suites. Each `MockX` (or production) impl runs
//! through these to verify it satisfies the trait's invariants.
//!
//! M1.23 shipped the `filesystem` suite. M2.07 adds the remaining 12 (`clock`,
//! `runtime`, `http`, `process`, `mcp`, `lsp`, `sandbox`, `worktree`,
//! `secure_storage`, `swarm`, `bridge`, `effect_handler`). All 13 traits now
//! have a contract.

pub mod bridge;
pub mod clock;
pub mod effect_handler;
pub mod filesystem;
pub mod http;
pub mod lsp;
pub mod mcp;
pub mod process;
pub mod runtime;
pub mod sandbox;
pub mod secure_storage;
pub mod swarm;
pub mod worktree;
