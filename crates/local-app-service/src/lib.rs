//! Local App service — the orchestration that sits between the shared core
//! ([`local_apps`]) and whatever hosts it.
//!
//! `local-apps` owns the data model, the on-disk stores and the state machine.
//! This crate owns what drives them:
//!
//! - [`broker`]: the one trust boundary for an app's data, files, device access,
//!   model calls, background work, runtime and approvals. The page bridge, the
//!   agent tools and the native client commands all reach an app through it.
//! - [`app_build`]: turning an app's workspace into a verified build, and
//!   judging whether an app's recorded build and dependencies can be trusted.
//! - [`mcp_server`]: the in-process MCP server that exposes the app library and
//!   each app's approved tools, and [`tool_names`], the names a model calls them
//!   by.
//! - [`runtime_profiles`], [`dependency_integrity`], [`template_catalog`] and
//!   [`plan_approval`]: what a runtime profile pins, how an installed dependency
//!   tree is verified, which templates a plan may choose, and what the person
//!   actually approved.
//!
//! The crate knows nothing about the engine that embeds it, and the compiler
//! keeps it that way: its production dependencies inside this workspace are the
//! shared primitives (`device-api`, `local-app-contracts`, `mcp-wire`,
//! `rooted-fs`) and `local-apps`, and `scripts/checks/check_deps.py` fails the
//! build of anything more. Everything a host has to provide (events, approvals,
//! device access, command execution, model calls, MCP publication) enters as a
//! trait defined here — see [`host`], [`publication`] and [`llm`] — and is
//! implemented by the host, never imported.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: the modules moved
// here from the runtime carried their `pub(crate)`/`pub(super)` surface
// undocumented. The lint stays `warn` at the workspace level so a NEW crate
// still inherits the requirement; this allow is scoped here so the debt is
// visible per crate and can be repaid by deleting this line.
#![allow(missing_docs)]

// `app_build`, `broker` and `mcp_server` moved from `runtime::mobile`, which
// carried a module-wide `#![allow(dead_code)]` over them. About fifteen private
// items in them are never used (mostly in `broker/authoring.rs`). Dead code is a
// finding to repair, not a decision, so the allow is per module: it cannot
// spread to the modules that were written here, and deleting it lists them.
#[allow(dead_code)]
pub mod app_build;
#[allow(dead_code)]
pub mod broker;
pub mod dependency_integrity;
pub mod device_capabilities;
pub mod host;
pub mod llm;
#[allow(dead_code)]
pub mod mcp_server;
pub mod plan_approval;
pub mod publication;
pub mod runtime_profiles;
pub mod template_catalog;
pub mod tool_names;
pub mod worker;

#[cfg(test)]
mod test_support;
