//! Schema-2 app library, independent of accounts and conversation lifetime.
//! Filesystem calls are synchronous: async hosts must use their blocking executor.
/// Host-owned Agent task records shared by native platforms.
pub mod agent;
mod library;
mod manifest;
mod model;
pub use library::*;
pub use manifest::*;
pub use model::*;

/// Errors returned by the local app library.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The request is invalid.
    #[error("invalid local app: {0}")]
    Invalid(String),
    /// The requested app was not found.
    #[error("local app not found: {0}")]
    NotFound(String),
    /// The requested revision conflicts with the current revision.
    #[error("publish conflict: expected revision {expected}, current revision {actual}")]
    Conflict {
        /// Revision supplied by the caller.
        expected: u64,
        /// Current revision in the library.
        actual: u64,
    },
    /// Execution consent is required.
    #[error("execution consent required for release {0}")]
    ConsentRequired(String),
    /// The app data schema is incompatible with the release.
    #[error("data schema {current} is not compatible with release schema {target}")]
    DataIncompatible {
        /// Existing app data schema version.
        current: u32,
        /// Schema version required by the release.
        target: u32,
    },
    /// A verified data migration is required.
    #[error("data schema change {current} → {target} requires a verified backup and migration transaction; existing data and current release are retained")]
    DataMigrationRequired {
        /// Existing app data schema version.
        current: u32,
        /// Schema version required by the release.
        target: u32,
    },
    /// The app is busy.
    #[error("app is busy: {0}")]
    Busy(String),
    /// A storage operation failed.
    #[error("local app storage: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error("local app JSON: {0}")]
    Json(#[from] serde_json::Error),
}
impl AppError {
    /// Return the stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) | Self::Json(_) => "invalidRequest",
            Self::NotFound(_) => "notFound",
            Self::Conflict { .. } => "revisionConflict",
            Self::ConsentRequired(_) => "consentRequired",
            Self::DataIncompatible { .. } => "dataIncompatible",
            Self::DataMigrationRequired { .. } => "dataMigrationRequired",
            Self::Busy(_) => "busy",
            Self::Io(_) => "storageFailure",
        }
    }
}

#[cfg(test)]
mod tests;
