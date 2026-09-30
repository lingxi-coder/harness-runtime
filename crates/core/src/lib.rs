//! Internal shared models, host contracts and conversation rules.
//!
//! Domain crates own their specific behavior. Shared values live in [`types`],
//! host capabilities in [`host`], and common conversation rules in the
//! remaining modules. [`settings`] also includes file-backed settings loading.

#![deny(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 4 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod events;
pub mod host;
pub mod prompt;
pub mod reducer;
pub mod session;
pub mod settings;
pub mod state_machine;
pub mod token;
pub mod types;

pub use events::Event;
pub use reducer::reduce;
pub use session::{CumulativeUsage, SessionState, TodoItem, TodoState};
pub use state_machine::ConversationState;
pub use token::Usage;
