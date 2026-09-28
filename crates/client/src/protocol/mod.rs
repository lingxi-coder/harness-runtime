//! Transport-independent client DTOs and protocol version.
//! JSON payload fields remain strings for native binding compatibility.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 69 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod ask_user_question;
pub mod audio;
pub mod commands;
pub mod computer_access;
pub mod controls;
pub mod error;
pub mod events;
pub mod listings;
pub mod local_apps;
pub mod message;
pub mod permission;
pub mod tool_display;
pub mod version;
