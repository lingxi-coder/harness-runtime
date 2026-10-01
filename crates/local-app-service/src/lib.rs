//! Local App service — the orchestration that sits between the shared core
//! ([`local_apps`]) and whatever hosts it.
//!
//! `local-apps` owns the data model, the on-disk stores and the state machine.
//! This crate owns what drives them: how a runtime profile pins a toolchain and
//! a dependency lock, how an installed dependency tree is verified, and — as
//! the extraction proceeds — the broker that runs, builds, publishes and
//! approves apps on behalf of a host.
//!
//! The crate knows nothing about the engine that embeds it. It reaches the
//! workspace only through the shared primitives `local-apps` already names;
//! `scripts/checks/check_deps.py` keeps it that way. Everything a host has to
//! provide (events, approvals, device access, command execution) will enter as
//! a trait defined here and implemented by the host, never as an import.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: the modules moved
// here from the runtime carried their `pub(crate)`/`pub(super)` surface
// undocumented. The lint stays `warn` at the workspace level so a NEW crate
// still inherits the requirement; this allow is scoped here so the debt is
// visible per crate and can be repaid by deleting this line.
#![allow(missing_docs)]

pub mod dependency_integrity;
pub mod host;
pub mod llm;
pub mod publication;
pub mod runtime_profiles;
