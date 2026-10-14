//! Client protocol, with optional runtime adapters and presentation.
#![forbid(unsafe_code)]

#[cfg(feature = "adapter")]
pub mod adapter;
#[cfg(feature = "presentation")]
pub mod presentation;
pub mod protocol;
#[cfg(feature = "slash-hints")]
mod slash_argument_hint;
#[cfg(feature = "slash-hints")]
pub use slash_argument_hint::slash_command_argument_count;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
