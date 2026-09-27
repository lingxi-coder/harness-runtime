//! Conversation state machine (cargo package `core`, rust ident `lingxi_core`).
//!
//! - `events::Event` — inputs to the reducer
//! - `state_machine::ConversationState` — the state set
//! - `reducer::reduce` — pure-function state transitions
//! - `prompt::assemble_request` — build the API request body
//! - `session::SessionState` — persisted session model
//!
//! No I/O. No `tokio::spawn`. All side effects are returned as
//! `protocol::Effect` values.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 4 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod events;
pub mod model;
pub mod prompt;
pub mod reducer;
pub mod session;
pub mod settings;
pub mod state_machine;
pub mod token;

pub use events::Event;
pub use reducer::reduce;
pub use session::{CumulativeUsage, SessionState, TodoItem, TodoState};
pub use state_machine::ConversationState;
pub use token::Usage;
