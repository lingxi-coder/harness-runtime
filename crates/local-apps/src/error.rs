//! Typed error surface for the local-apps core.
//!
//! Every variant maps to exactly one wire-level [`AppErrorCode`] so the engine
//! can surface failures as a typed `AppOperationFailed { code, message }`
//! client event. LSP diagnostic messages use a source projection because they
//! originate in arbitrary server text.

use lingxi_core::host::FsError;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
use thiserror::Error;

fn source_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text derives from the exact UTF-16 units")
}

fn prefixed_source(prefix: &str, source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(
        source
            .string_units("")
            .expect("LSP diagnostic message is a root string"),
    );
    source_from_units(units)
}

/// Stable machine-readable failure codes carried on `AppOperationFailed`
/// client events. Extensible in later phases; each [`AppError`] variant maps
/// to exactly one code via [`AppError::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppErrorCode {
    /// The addressed app (or sub-resource) does not exist.
    NotFound,
    /// A caller-supplied revision does not match the current draft revision.
    RevisionConflict,
    /// A caller-supplied interaction/suggestion id is not the pending one.
    InteractionInvalid,
    /// The operation is not legal in the app's current workflow state.
    WorkflowStateInvalid,
    /// The app runtime is in a state that blocks the operation.
    RuntimeBusy,
    /// The capability is gated behind a later phase (runtime = phase 4,
    /// git checkpoints = phase 5).
    NotYetAvailable,
    /// Persisted state failed to parse or violates invariants.
    StorageCorrupt,
    /// The request itself is malformed (bad id, empty name, …).
    InvalidRequest,
    /// Fresh LSP diagnostics blocked the requested build or authoring step.
    LspDiagnosticsFailed,
    /// Underlying I/O failure.
    Io,
    /// The model is unreachable: offline, unauthenticated, or timed out.
    LlmUnavailable,
    /// The model's output failed validation (bad shape or over a limit).
    LlmOutputRejected,
}

/// Error type for every fallible local-apps operation.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum AppError {
    /// The addressed app does not exist.
    #[error("app not found: {0}")]
    NotFound(String),
    /// The caller's `expected_revision` does not match the draft's current
    /// revision. The user value is never silently overwritten.
    #[error("revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict {
        /// Revision the caller believed to be current.
        expected: u64,
        /// Revision the draft actually holds.
        actual: u64,
    },
    /// The interaction (or suggestion) id is not the pending one for the app.
    #[error("interaction invalid: {0}")]
    InteractionInvalid(String),
    /// The transition is not legal from the app's current workflow state.
    #[error("workflow state invalid: {0}")]
    WorkflowStateInvalid(String),
    /// The app runtime is busy (starting/running/stopping).
    #[error("runtime busy: {0}")]
    RuntimeBusy(String),
    /// The capability lands in a later phase.
    #[error("not yet available: {0}")]
    NotYetAvailable(String),
    /// On-disk state failed to parse or violates invariants.
    #[error("storage corrupt: {0}")]
    StorageCorrupt(String),
    /// The request is malformed.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Recent LSP diagnostics blocked the operation.
    #[error("lsp diagnostics failed: {}", .0.string_at("").unwrap_or_default())]
    LspDiagnosticsFailed(
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")] Utf16JsonProjection,
    ),
    /// Underlying I/O failure.
    #[error("io error: {0}")]
    Io(String),
    /// The model is unreachable. There is no template to fall back to —
    /// this design deliberately leaves no silent degradation path.
    #[error("llm unavailable: {0}")]
    LlmUnavailable(String),
    /// The model's output was rejected by a validator.
    #[error("llm output rejected: {0}")]
    LlmOutputRejected(String),
}

impl AppError {
    /// Wire-level code for this error.
    #[must_use]
    pub fn code(&self) -> AppErrorCode {
        match self {
            Self::NotFound(_) => AppErrorCode::NotFound,
            Self::RevisionConflict { .. } => AppErrorCode::RevisionConflict,
            Self::InteractionInvalid(_) => AppErrorCode::InteractionInvalid,
            Self::WorkflowStateInvalid(_) => AppErrorCode::WorkflowStateInvalid,
            Self::RuntimeBusy(_) => AppErrorCode::RuntimeBusy,
            Self::NotYetAvailable(_) => AppErrorCode::NotYetAvailable,
            Self::StorageCorrupt(_) => AppErrorCode::StorageCorrupt,
            Self::InvalidRequest(_) => AppErrorCode::InvalidRequest,
            Self::LspDiagnosticsFailed(_) => AppErrorCode::LspDiagnosticsFailed,
            Self::Io(_) => AppErrorCode::Io,
            Self::LlmUnavailable(_) => AppErrorCode::LlmUnavailable,
            Self::LlmOutputRejected(_) => AppErrorCode::LlmOutputRejected,
        }
    }

    /// Source-native model/client message for this error.
    ///
    /// LSP diagnostic text remains exact UTF-16. Other variants contain
    /// host-generated or already-typed Rust strings and retain their current
    /// display wording when projected.
    #[must_use]
    pub fn source_message(&self) -> Utf16JsonProjection {
        match self {
            Self::NotFound(value) => format!("app not found: {value}").into(),
            Self::RevisionConflict { expected, actual } => {
                format!("revision conflict: expected {expected}, actual {actual}").into()
            }
            Self::InteractionInvalid(value) => format!("interaction invalid: {value}").into(),
            Self::WorkflowStateInvalid(value) => format!("workflow state invalid: {value}").into(),
            Self::RuntimeBusy(value) => format!("runtime busy: {value}").into(),
            Self::NotYetAvailable(value) => format!("not yet available: {value}").into(),
            Self::StorageCorrupt(value) => format!("storage corrupt: {value}").into(),
            Self::InvalidRequest(value) => format!("invalid request: {value}").into(),
            Self::LspDiagnosticsFailed(source) => {
                prefixed_source("lsp diagnostics failed: ", source)
            }
            Self::Io(value) => format!("io error: {value}").into(),
            Self::LlmUnavailable(value) => format!("llm unavailable: {value}").into(),
            Self::LlmOutputRejected(value) => format!("llm output rejected: {value}").into(),
        }
    }

    /// Map a filesystem error raised while performing `op`.
    ///
    /// Lexical containment violations surface as [`AppError::InvalidRequest`]
    /// (a path tried to escape the apps root); everything else is I/O.
    #[must_use]
    pub fn from_fs(op: &str, err: &FsError) -> Self {
        match err {
            FsError::OutsideWorkspace(path) => {
                Self::InvalidRequest(format!("{op}: path escapes the apps root: {path}"))
            }
            other => Self::Io(format!("{op}: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_its_code() {
        assert_eq!(
            AppError::NotFound("x".into()).code(),
            AppErrorCode::NotFound
        );
        assert_eq!(
            AppError::RevisionConflict {
                expected: 1,
                actual: 2
            }
            .code(),
            AppErrorCode::RevisionConflict
        );
        assert_eq!(
            AppError::InteractionInvalid("x".into()).code(),
            AppErrorCode::InteractionInvalid
        );
        assert_eq!(
            AppError::WorkflowStateInvalid("x".into()).code(),
            AppErrorCode::WorkflowStateInvalid
        );
        assert_eq!(
            AppError::RuntimeBusy("x".into()).code(),
            AppErrorCode::RuntimeBusy
        );
        assert_eq!(
            AppError::NotYetAvailable("x".into()).code(),
            AppErrorCode::NotYetAvailable
        );
        assert_eq!(
            AppError::StorageCorrupt("x".into()).code(),
            AppErrorCode::StorageCorrupt
        );
        assert_eq!(
            AppError::InvalidRequest("x".into()).code(),
            AppErrorCode::InvalidRequest
        );
        assert_eq!(
            AppError::LspDiagnosticsFailed("x".into()).code(),
            AppErrorCode::LspDiagnosticsFailed
        );
        assert_eq!(AppError::Io("x".into()).code(), AppErrorCode::Io);
        assert_eq!(
            AppError::LlmUnavailable("x".into()).code(),
            AppErrorCode::LlmUnavailable
        );
        assert_eq!(
            AppError::LlmOutputRejected("x".into()).code(),
            AppErrorCode::LlmOutputRejected
        );
    }

    #[test]
    fn codes_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&AppErrorCode::RevisionConflict).unwrap(),
            "\"revision_conflict\""
        );
        assert_eq!(
            serde_json::to_string(&AppErrorCode::NotYetAvailable).unwrap(),
            "\"not_yet_available\""
        );
        assert_eq!(
            serde_json::to_string(&AppErrorCode::WorkflowStateInvalid).unwrap(),
            "\"workflow_state_invalid\""
        );
        assert_eq!(
            serde_json::to_string(&AppErrorCode::LlmUnavailable).unwrap(),
            "\"llm_unavailable\""
        );
        assert_eq!(
            serde_json::to_string(&AppErrorCode::LspDiagnosticsFailed).unwrap(),
            "\"lsp_diagnostics_failed\""
        );
        assert_eq!(
            serde_json::to_string(&AppErrorCode::LlmOutputRejected).unwrap(),
            "\"llm_output_rejected\""
        );
    }

    #[test]
    fn outside_workspace_maps_to_invalid_request() {
        let err = AppError::from_fs("delete", &FsError::OutsideWorkspace("../x".into()));
        assert_eq!(err.code(), AppErrorCode::InvalidRequest);
        let err = AppError::from_fs("read", &FsError::Io("boom".into()));
        assert_eq!(err.code(), AppErrorCode::Io);
    }

    #[test]
    fn lsp_diagnostic_error_retains_exact_text_through_message_and_serde() {
        let diagnostic = Utf16JsonProjection::parse(r#""vite: failed \ud800""#)
            .expect("LSP message is valid JSON string source");
        let error = AppError::LspDiagnosticsFailed(diagnostic.clone());
        let mut expected = "lsp diagnostics failed: "
            .encode_utf16()
            .collect::<Vec<_>>();
        expected.extend(diagnostic.string_units("").unwrap());
        assert_eq!(
            error.source_message().string_units(""),
            Some(expected.clone())
        );

        let serialized = serde_json::to_string(&error).expect("serialize source error");
        assert!(serialized.contains(r#"\ud800"#));
        let restored: AppError = serde_json::from_str(&serialized).expect("restore source error");
        let AppError::LspDiagnosticsFailed(restored) = restored else {
            panic!("expected LSP diagnostic error");
        };
        assert_eq!(
            restored.string_units(""),
            Some(diagnostic.string_units("").unwrap())
        );
    }
}
