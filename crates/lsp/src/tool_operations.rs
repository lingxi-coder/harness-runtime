//! The nine `LSPTool` operations as pure functions over an `LspClient`.
//!
//! Each operation:
//! 1. Validates 1-based `line` / `character` (reject zero or negative).
//! 2. Resolves the file URI from an absolute or workspace-relative path.
//! 3. Gates `textDocument/didOpen` through [`OpenFileTracker`] — file is
//!    read from disk on first request per `(server, uri)` pair.
//! 4. Enforces [`MAX_LSP_FILE_SIZE_BYTES`] (10 MB) on the file size.
//! 5. Converts 1-based positions to 0-based on the LSP wire.
//! 6. Issues the appropriate `textDocument/...` request and returns the
//!    raw JSON `result` for the caller to format.
//!
//! Reference: `claude-code/src/tools/LSPTool/LSPTool.ts`,
//! specifically `MAX_LSP_FILE_SIZE_BYTES = 10_000_000` (line 53) and the
//! `getMethodAndParams` switch (lines 427-513).

use crate::client::LspClient;
use crate::open_file_tracker::{DocumentSync, OpenFileTracker};
use crate::path_mapper::LspDocumentPath;
use lingxi_core::host::LspServerConfig;
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, DocumentSymbolParams, Position, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkDoneProgressParams,
    WorkspaceSymbolParams,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;
use tokio::fs;

/// Upper bound on LSP-eligible file size (10 MB). Matches claude-code's
/// `MAX_LSP_FILE_SIZE_BYTES = 10_000_000` constant verbatim.
pub const MAX_LSP_FILE_SIZE_BYTES: u64 = 10_000_000;

/// The nine operations `LSPTool` exposes. Names mirror claude-code's
/// `operation` enum (`LSPTool.ts:60-72`) verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LspOperation {
    /// `textDocument/definition`.
    GoToDefinition,
    /// `textDocument/references`.
    FindReferences,
    /// `textDocument/hover`.
    Hover,
    /// `textDocument/completion` (M4-07 additive).
    Completion,
    /// `textDocument/documentSymbol`.
    DocumentSymbol,
    /// `workspace/symbol`.
    WorkspaceSymbol,
    /// `textDocument/implementation`.
    GoToImplementation,
    /// `textDocument/prepareCallHierarchy`.
    PrepareCallHierarchy,
    /// `callHierarchy/incomingCalls` (two-step round-trip).
    IncomingCalls,
    /// `callHierarchy/outgoingCalls` (two-step round-trip).
    OutgoingCalls,
}

/// Wrapped result from one operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspOperationResult {
    /// Operation that produced this result.
    pub operation: LspOperation,
    /// File the operation was performed on (URI string form; empty for
    /// workspace-symbol).
    pub file_uri: String,
    /// Raw JSON `result` (decoded by caller into operation-specific shapes).
    pub raw: Value,
}

/// Failures the operations can produce.
#[derive(Debug, Clone, Error)]
pub enum LspOperationError {
    /// 1-based `line` or `character` was zero or negative.
    #[error("position must be 1-based (line and character >= 1); got line={line}, character={character}")]
    InvalidPosition {
        /// Caller-supplied 1-based line (recorded as i64 so we can echo
        /// values such as `0` that fail the `>= 1` precondition).
        line: i64,
        /// Caller-supplied 1-based character.
        character: i64,
    },
    /// File path could not be canonicalized into a `file://` URL.
    #[error("invalid file path: {0}")]
    InvalidPath(String),
    /// File does not exist or cannot be stat-ed.
    #[error("file io: {0}")]
    Io(String),
    /// File exceeds [`MAX_LSP_FILE_SIZE_BYTES`].
    #[error("file too large for LSP analysis ({size}B exceeds {limit}B limit)")]
    FileTooLarge {
        /// Actual size of the rejected file in bytes.
        size: u64,
        /// The constant ceiling — [`MAX_LSP_FILE_SIZE_BYTES`].
        limit: u64,
    },
    /// File content was not valid UTF-8.
    #[error("file is not valid utf-8: {0}")]
    NotUtf8(String),
    /// Underlying LSP transport / server error.
    #[error("{0}")]
    Lsp(#[from] lingxi_core::host::LspError),
}

/// Convert a 1-based UI position to the 0-based LSP wire position.
///
/// Returns [`LspOperationError::InvalidPosition`] when either coordinate
/// is `< 1` (claude-code rejects these in the input schema before they
/// reach the dispatcher).
pub fn position_from_one_based(line: u32, character: u32) -> Result<Position, LspOperationError> {
    if line == 0 || character == 0 {
        return Err(LspOperationError::InvalidPosition {
            line: i64::from(line),
            character: i64::from(character),
        });
    }
    Ok(Position {
        line: line - 1,
        character: character - 1,
    })
}

/// Build a `file://` URL from an absolute path.
pub fn uri_from_path(path: &Path) -> Result<Url, LspOperationError> {
    Url::from_file_path(path).map_err(|()| {
        LspOperationError::InvalidPath(format!("cannot build file:// URL from {}", path.display()))
    })
}

#[doc(hidden)]
pub trait LspDocumentArg {
    fn as_document_path(&self) -> Result<Cow<'_, LspDocumentPath>, LspOperationError>;
}

impl LspDocumentArg for LspDocumentPath {
    fn as_document_path(&self) -> Result<Cow<'_, LspDocumentPath>, LspOperationError> {
        Ok(Cow::Borrowed(self))
    }
}

impl LspDocumentArg for Path {
    fn as_document_path(&self) -> Result<Cow<'_, LspDocumentPath>, LspOperationError> {
        Ok(Cow::Owned(identity_document_path(self)?))
    }
}

impl LspDocumentArg for PathBuf {
    fn as_document_path(&self) -> Result<Cow<'_, LspDocumentPath>, LspOperationError> {
        self.as_path().as_document_path()
    }
}

fn identity_document_path(path: &Path) -> Result<LspDocumentPath, LspOperationError> {
    Ok(LspDocumentPath {
        host_path: path.to_path_buf(),
        server_path: path.to_path_buf(),
        uri: uri_from_path(path)?,
    })
}

/// Look up the LSP `languageId` for `file_path` from `server_config`. Falls
/// back to `"plaintext"` (claude-code default) when the extension is
/// unknown.
#[must_use]
pub fn language_id_for(server_config: &LspServerConfig, file_path: &Path) -> String {
    let ext_with_dot = file_path
        .extension()
        .and_then(|os| os.to_str())
        .map(|s| format!(".{}", s.to_lowercase()))
        .unwrap_or_default();
    server_config
        .extension_to_language
        .get(&ext_with_dot)
        .cloned()
        .unwrap_or_else(|| "plaintext".to_string())
}

/// Synchronize `file_path` from disk before a direct LSP operation.
async fn ensure_did_open(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &LspDocumentPath,
) -> Result<(), LspOperationError> {
    let meta = fs::metadata(&document.host_path).await.map_err(|e| {
        LspOperationError::Io(format!("stat {}: {}", document.host_path.display(), e))
    })?;
    let size = meta.len();
    if size > MAX_LSP_FILE_SIZE_BYTES {
        return Err(LspOperationError::FileTooLarge {
            size,
            limit: MAX_LSP_FILE_SIZE_BYTES,
        });
    }
    let bytes = fs::read(&document.host_path).await.map_err(|e| {
        LspOperationError::Io(format!("read {}: {}", document.host_path.display(), e))
    })?;
    let text = String::from_utf8(bytes).map_err(|e| LspOperationError::NotUtf8(e.to_string()))?;
    sync_document_text(client, tracker, config, document, &text)
        .await
        .map(|_| ())
}

/// Synchronize already-available UTF-8 text with one LSP server.
///
/// The shared tracker chooses `didOpen`, full-text `didChange` + `didSave`, or
/// no notification for byte-identical content. This is reused by direct LSP
/// operations and by successful Edit/Write calls.
pub(crate) async fn sync_document_text(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &LspDocumentPath,
    text: &str,
) -> Result<DocumentSync, LspOperationError> {
    let language_id = language_id_for(config, &document.server_path);
    let _sync_guard = tracker.lock_sync().await;
    let connection = client.connection();
    tracker
        .register_connection(client.name(), connection.clone())
        .await;
    if !tracker
        .is_active_connection(client.name(), &connection)
        .await
    {
        return Err(LspOperationError::Lsp(
            lingxi_core::host::LspError::Unavailable,
        ));
    }

    let sync = tracker
        .plan_sync(client.name(), document.uri.clone(), text)
        .await;
    match &sync {
        DocumentSync::Open { version, evicted } => {
            client
                .notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": document.uri,
                            "languageId": language_id,
                            "version": version,
                            "text": text,
                        }
                    }),
                )
                .await?;
            for (server_name, evicted_uri) in evicted {
                let owner = if server_name == client.name() {
                    Some(connection.clone())
                } else {
                    tracker.connection_for_server(server_name).await
                };
                if let Some(owner) = owner {
                    if let Err(error) = owner.notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": evicted_uri } }),
                    ) {
                        tracing::warn!(
                            target: "lingxi_lsp::client",
                            server = %server_name,
                            %error,
                            "LSP didClose notification failed; continuing"
                        );
                    }
                }
            }
        }
        DocumentSync::Change { version } => {
            client
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": document.uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
            client
                .notify(
                    "textDocument/didSave",
                    json!({ "textDocument": { "uri": document.uri } }),
                )
                .await?;
        }
        DocumentSync::Unchanged => {}
    }
    Ok(sync)
}

/// `textDocument/hover` at a 1-based position.
///
/// # Errors
/// Position validation, I/O on the source file, file-size cap, UTF-8
/// decode, and JSON-RPC transport / server errors can all surface here.
pub async fn hover<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let position = position_from_one_based(line, character)?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    };
    let raw: Value = client.request("textDocument/hover", params).await?;
    Ok(LspOperationResult {
        operation: LspOperation::Hover,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `textDocument/definition` at a 1-based position.
///
/// # Errors
/// Same set as [`hover`].
pub async fn go_to_definition<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let position = position_from_one_based(line, character)?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    };
    let raw: Value = client.request("textDocument/definition", params).await?;
    Ok(LspOperationResult {
        operation: LspOperation::GoToDefinition,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `textDocument/references` at a 1-based position.
///
/// `include_declaration` matches claude-code's hard-coded `true` (see
/// `LSPTool.ts:453`).
///
/// # Errors
/// Same set as [`hover`].
pub async fn find_references<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let position = position_from_one_based(line, character)?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
        context: ReferenceContext {
            include_declaration,
        },
    };
    let raw: Value = client.request("textDocument/references", params).await?;
    Ok(LspOperationResult {
        operation: LspOperation::FindReferences,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `textDocument/completion` at a 1-based position (M4-07 additive).
///
/// # Errors
/// Same set as [`hover`].
pub async fn completion<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let position = position_from_one_based(line, character)?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    };
    let raw: Value = client.request("textDocument/completion", params).await?;
    Ok(LspOperationResult {
        operation: LspOperation::Completion,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `textDocument/documentSymbol` for the whole file.
///
/// # Errors
/// Same set as [`hover`] minus the position-validation case (this
/// operation does not consume a position).
pub async fn document_symbol<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    let raw: Value = client
        .request("textDocument/documentSymbol", params)
        .await?;
    Ok(LspOperationResult {
        operation: LspOperation::DocumentSymbol,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `workspace/symbol` with a fuzzy query (empty string returns everything;
/// matches claude-code's default).
///
/// Does not require any file to be opened; we do not consult the tracker.
///
/// # Errors
/// JSON-RPC transport / server errors only.
pub async fn workspace_symbol(
    client: &LspClient,
    query: Option<String>,
) -> Result<LspOperationResult, LspOperationError> {
    let params = WorkspaceSymbolParams {
        query: query.unwrap_or_default(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    let raw: Value = client.request("workspace/symbol", params).await?;
    Ok(LspOperationResult {
        operation: LspOperation::WorkspaceSymbol,
        file_uri: String::new(),
        raw,
    })
}

/// `textDocument/implementation` at a 1-based position.
///
/// # Errors
/// Same set as [`hover`].
pub async fn go_to_implementation<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let position = position_from_one_based(line, character)?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    };
    let raw: Value = client
        .request("textDocument/implementation", params)
        .await?;
    Ok(LspOperationResult {
        operation: LspOperation::GoToImplementation,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `textDocument/prepareCallHierarchy` at a 1-based position.
///
/// # Errors
/// Same set as [`hover`].
pub async fn prepare_call_hierarchy<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let document = document.as_document_path()?;
    let position = position_from_one_based(line, character)?;
    let uri = document.uri.clone();
    ensure_did_open(client, tracker, config, document.as_ref()).await?;
    let params = CallHierarchyPrepareParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let raw: Value = client
        .request("textDocument/prepareCallHierarchy", params)
        .await?;
    Ok(LspOperationResult {
        operation: LspOperation::PrepareCallHierarchy,
        file_uri: uri.to_string(),
        raw,
    })
}

/// `callHierarchy/incomingCalls` — two-step: prepare, then fetch incoming
/// calls for the first item. Matches claude-code's `LSPTool.ts:317-326`.
///
/// # Errors
/// Same set as [`hover`].
pub async fn incoming_calls<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let prepare =
        prepare_call_hierarchy(client, tracker, config, document, line, character).await?;
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare.raw.clone()).unwrap_or_default();
    let Some(item) = items.into_iter().next() else {
        return Ok(LspOperationResult {
            operation: LspOperation::IncomingCalls,
            file_uri: prepare.file_uri,
            raw: json!([]),
        });
    };
    let params = CallHierarchyIncomingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    let raw: Value = client
        .request("callHierarchy/incomingCalls", params)
        .await?;
    Ok(LspOperationResult {
        operation: LspOperation::IncomingCalls,
        file_uri: prepare.file_uri,
        raw,
    })
}

/// `callHierarchy/outgoingCalls` — same two-step structure as
/// [`incoming_calls`].
///
/// # Errors
/// Same set as [`hover`].
pub async fn outgoing_calls<D: LspDocumentArg + ?Sized>(
    client: &LspClient,
    tracker: &OpenFileTracker,
    config: &LspServerConfig,
    document: &D,
    line: u32,
    character: u32,
) -> Result<LspOperationResult, LspOperationError> {
    let prepare =
        prepare_call_hierarchy(client, tracker, config, document, line, character).await?;
    let items: Vec<CallHierarchyItem> =
        serde_json::from_value(prepare.raw.clone()).unwrap_or_default();
    let Some(item) = items.into_iter().next() else {
        return Ok(LspOperationResult {
            operation: LspOperation::OutgoingCalls,
            file_uri: prepare.file_uri,
            raw: json!([]),
        });
    };
    let params = CallHierarchyOutgoingCallsParams {
        item,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    let raw: Value = client
        .request("callHierarchy/outgoingCalls", params)
        .await?;
    Ok(LspOperationResult {
        operation: LspOperation::OutgoingCalls,
        file_uri: prepare.file_uri,
        raw,
    })
}
