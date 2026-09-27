//! LSP (Language Server Protocol) registry and core primitives.
//!
//! M1.18 ships the engine-side surface: per-server state machine
//! ([`LspConnectionState`]), routing registry ([`LspRegistry`]), action
//! payload ([`LspAction`]).
//!
//! The `Tool`-implementing `LSPTool` lives in `lingxi-tools` (M4-07) to
//! break a `lingxi-lsp → lingxi-tools` cycle now that M4-07 needs
//! `lingxi-tools → lingxi-lsp` for `LspClient` + `tool_operations`.
//!
//! See spec §25 (LSP).

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 36 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod action;
pub mod client;
pub mod config;
pub mod connection;
pub mod diagnostic_registry;
pub mod diagnostics_format;
pub mod open_file_tracker;
pub mod passive_feedback;
pub mod path_mapper;
pub mod registry;
pub mod tool_operations;
pub mod transport;

pub use action::{LspAction, LspResponse};
pub use client::LspClient;
pub use connection::LspConnectionState;
pub use diagnostic_registry::{DiagnosticEntry, LspDiagnosticRegistry};
pub use lsp_types::Url;
pub use open_file_tracker::{OpenFileTracker, MAX_OPEN_DOCUMENTS};
pub use passive_feedback::PassiveDiagnosticSubscriber;
pub use path_mapper::{DesktopLspPathMapper, LspDocumentPath, LspPathMapper};
pub use registry::{LspActivationMode, LspRegistry};
pub use tool_operations::{
    LspOperation, LspOperationError, LspOperationResult, MAX_LSP_FILE_SIZE_BYTES,
};
