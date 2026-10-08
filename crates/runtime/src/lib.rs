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

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod effort_settings;
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod fast_settings;
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod skill_prompt;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod parked_agent_restore;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod main_report_waker;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod instruction_identity;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod model_resolution;

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

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
