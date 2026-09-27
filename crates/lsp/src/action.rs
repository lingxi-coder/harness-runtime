//! LSP tool action payload.
//!
//! `LspTool` accepts a single tagged-union JSON object describing which
//! LSP capability the model wants to invoke. `LspAction` is the on-wire
//! representation and `LspResponse` is the opaque JSON return value.
//!
//! See spec §25.4 (LSP tool).

use serde::{Deserialize, Serialize};

/// One invocation of the `LSP` tool.
///
/// The discriminator key is `action` and values are snake-cased (matching
/// the LSP method namespace).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum LspAction {
    /// `textDocument/hover` at a position.
    Hover {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/definition` at a position.
    Definition {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/references` at a position.
    References {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/publishDiagnostics` — most recent diagnostics for the
    /// active document.
    Diagnostics,
    /// `workspace/symbol` (when `query` is `Some`) or
    /// `textDocument/documentSymbol` (when `query` is `None`).
    Symbols {
        /// Optional fuzzy filter.
        query: Option<String>,
    },
    /// `textDocument/completion` at a position.
    Completion {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/formatting` on the active document.
    Formatting,
    /// `textDocument/rename` on the symbol at a position.
    Rename {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
        /// New identifier to apply.
        new_name: String,
    },
}

/// Opaque JSON value returned by an LSP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspResponse(
    /// Raw JSON-RPC `result` payload.
    pub serde_json::Value,
);
