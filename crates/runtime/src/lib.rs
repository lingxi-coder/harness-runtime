//! Public Harness SDK and platform composition profiles.
//!
//! Hosts use [`Harness`] and [`SessionHandle`] for session execution and
//! lifecycle. Desktop and mobile features select their platform assemblies.

#![forbid(unsafe_code)]

#[cfg(feature = "engine")]
pub mod api;
#[cfg(feature = "engine")]
pub use api::{
    CancellationToken, ConversationMessage, CostSnapshot, HandleError, Harness, HarnessBuilder,
    LifecycleService, OutputStream, RunInput, SessionHandle, SessionId, SessionService,
    ShutdownReport, TurnOutcome,
};
pub mod models;

#[cfg(feature = "collaboration")]
pub use coordinator;

#[cfg(feature = "fusion")]
pub use fusion;
#[cfg(feature = "workflow")]
pub use workflow;

/// Existing desktop product assembly.
#[cfg(feature = "desktop")]
pub mod desktop;
/// Build an embedded desktop Harness from the production composition.
#[cfg(feature = "desktop")]
pub use desktop::build_harness;
/// Existing mobile product assembly, usable without UniFFI.
#[cfg(feature = "mobile")]
pub mod mobile;

/// The Local App rows for the permission crate's per-tool table.
#[cfg(any(feature = "mobile", feature = "desktop"))]
pub(crate) mod local_app_tool_policy;
/// The Local App workspace profile for the permission crate's leases; both compositions install it.
#[cfg(any(feature = "mobile", feature = "desktop"))]
pub(crate) mod local_app_workspace_profile;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
