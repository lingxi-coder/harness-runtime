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

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub use provider_config::startup_catalog::{CatalogPlatform, StartupCatalog};

#[cfg(feature = "engine")]
pub mod api;
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod media_handoff;
#[cfg(any(feature = "desktop", feature = "mobile"))]
pub(crate) mod media_skill;
#[cfg(feature = "engine")]
pub use api::{
    CancellationToken, ConversationMessage, CostSnapshot, HandleError, Harness, HarnessBuilder,
    LifecycleService, OutputStream, RunInput, SessionHandle, SessionId, SessionService,
    ShutdownReport, TurnOutcome,
};
/// Shared bounded Local App Agent attachment decoding for desktop and mobile.
#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod app_agent_input;
/// Shared immutable Local App Skill verification and workspace materialization.
#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod app_agent_skills;
pub mod models;
/// Unified media composition usable without a conversation engine.
pub use media_runtime as media;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod bundled_skills_policy;
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod effort_settings;
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod fast_settings;
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod skill_prompt;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod inline_visualization;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod local_apps;

#[cfg(feature = "desktop")]
pub mod desktop_visualization;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod parked_agent_restore;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod session_agent_transcript;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod main_report_waker;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod instruction_identity;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod insights_corpus;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod insights;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod team_onboarding;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod list_agents_support;

#[cfg(feature = "desktop")]
mod insights_bun_worker;

#[cfg(feature = "desktop")]
mod bun_policy_js_semantics_worker;

#[cfg(any(feature = "desktop", feature = "mobile"))]
mod model_resolution;
#[cfg(any(feature = "desktop", feature = "mobile", test))]
mod native_model_settings;

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

/// Host-owned Bun/Ajv service for source-preserving JSON Schema validation.
#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod json_schema_validator;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

/// Host-owned, RAM-only independent side conversations.
#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod side_chat;

/// Keep Workflow's validated catalog snapshot in sync with the live slash registry.
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod workflow_catalog;
/// Native-shaped slash-command projection for the validated workflow catalog.
#[cfg(any(feature = "desktop", feature = "mobile"))]
mod workflow_slash;
