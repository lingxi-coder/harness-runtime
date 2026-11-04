//! Schema-2 app library, independent of accounts and conversation lifetime.
//! Filesystem calls are synchronous: async hosts must use their blocking executor.
/// Host-owned Agent task records shared by native platforms.
pub mod agent;
mod document;
mod library;
mod manifest;
mod market_remix;
mod model;
pub use document::validate_inline_document;
pub use library::*;
pub use manifest::*;
pub use market_remix::*;
pub use model::*;

/// Validation, consent, publication, and storage failures from the device library.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// An input or stored value violates the library contract.
    #[error("invalid local app: {0}")]
    Invalid(String),
    /// The requested app or release does not exist or has been removed.
    #[error("local app not found: {0}")]
    NotFound(String),
    /// A publication was prepared against a different library revision.
    #[error("publish conflict: expected revision {expected}, current revision {actual}")]
    Conflict {
        /// Revision supplied by the publishing caller.
        expected: u64,
        /// Revision currently stored in the library.
        actual: u64,
    },
    /// The release digest needs native execution consent before it may run.
    #[error("execution consent required for release {0}")]
    ConsentRequired(String),
    /// The release does not accept the app's current data schema.
    #[error("data schema {current} is not compatible with release schema {target}")]
    DataIncompatible {
        /// Schema of the retained app data.
        current: u32,
        /// Schema declared by the requested release.
        target: u32,
    },
    /// Changing the app's data schema requires a separate verified migration.
    #[error(
        "data schema change {current} → {target} requires a verified backup and migration transaction; existing data and current release are retained"
    )]
    DataMigrationRequired {
        /// Schema of the retained app data.
        current: u32,
        /// Schema declared by the requested release.
        target: u32,
    },
    /// A live run or another owner prevents the requested operation.
    #[error("app is busy: {0}")]
    Busy(String),
    /// The Chat creation ID was already used for different input.
    #[error("Chat creation request is already bound to different input")]
    ChatCreateConflict,
    /// The Chat creation ID was previously deleted and cannot be reused.
    #[error("Chat creation request refers to a deleted Chat")]
    ChatCreateDeleted,
    /// A Chat index write may have committed and requires reopening to verify.
    #[error("Chat storage outcome is uncertain; reopen the store before continuing")]
    ChatStorageUncertain,
    /// A not-yet-committed Market remix needs its original archive again.
    #[error("the original Market archive is missing")]
    ArchiveMissing,
    /// A filesystem operation failed.
    #[error("local app storage: {0}")]
    Io(#[from] std::io::Error),
    /// A serialized request or stored record could not be decoded or encoded.
    #[error("local app JSON: {0}")]
    Json(#[from] serde_json::Error),
}
impl AppError {
    /// Return the stable error code exposed by the native library interface.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) | Self::Json(_) => "invalidRequest",
            Self::NotFound(_) => "notFound",
            Self::Conflict { .. } => "revisionConflict",
            Self::ConsentRequired(_) => "consentRequired",
            Self::DataIncompatible { .. } => "dataIncompatible",
            Self::DataMigrationRequired { .. } => "dataMigrationRequired",
            Self::Busy(_) => "busy",
            Self::ChatCreateConflict => "chatCreateConflict",
            Self::ChatCreateDeleted => "chatCreateDeleted",
            Self::ChatStorageUncertain => "chatStorageUncertain",
            Self::ArchiveMissing => "archiveMissing",
            Self::Io(_) => "storageFailure",
        }
    }
}

#[cfg(test)]
mod tests;
