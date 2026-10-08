//! Public Harness SDK and platform composition profiles.
//!
//! Hosts use [`Harness`] and [`SessionHandle`] for session execution and
//! lifecycle. Desktop and mobile features select their platform assemblies.

#![forbid(unsafe_code)]

/// Supported stack budget for host threads polling complete Harness sessions.
/// Configure Tokio workers with this size when embedding the desktop/headless
/// runtime. The debug execution pipeline exceeds Tokio's default 2 MiB stack;
/// the mobile host uses the same 8 MiB budget. This reserves address space,
/// rather than committing the entire allocation as resident memory.
pub const RUNTIME_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;

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
/// Claude-compatible headless sessions with host-owned byte I/O.
#[cfg(feature = "desktop")]
pub mod headless;
/// Build an embedded desktop Harness from the production composition.
#[cfg(feature = "desktop")]
pub use desktop::build_harness;
/// Existing mobile product assembly, usable without UniFFI.
#[cfg(feature = "mobile")]
pub mod mobile;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
