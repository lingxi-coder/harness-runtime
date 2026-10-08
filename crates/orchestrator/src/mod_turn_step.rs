//! Shared Claude Code `turn.step` event adapter.
//!
//! The implementation lives in `llm-runtime` so both the main conversation
//! and child-agent loops use one ref table and one decoder.

pub use llm_runtime::mod_turn_step::{TurnStepDecoder, TurnStepEncoder};
