//! Client protocol, with optional runtime adapters and presentation.
#![forbid(unsafe_code)]

#[cfg(feature = "adapter")]
pub mod adapter;
#[cfg(feature = "presentation")]
pub mod presentation;
pub mod protocol;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
