//! LingXi's existing Harness components and platform composition profiles.
//!
//! The core profile exposes the same tested execution components without a UI
//! or foreign-binding requirement. Desktop and mobile select their existing
//! product assembly; neither introduces a second agent loop or state store.

#![forbid(unsafe_code)]

#[cfg(feature = "core")]
pub mod api;
#[cfg(feature = "core")]
pub use api::{Harness, HarnessBuilder, SessionHandle};
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
/// Existing mobile product assembly, usable without UniFFI.
#[cfg(feature = "mobile")]
pub mod mobile;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
