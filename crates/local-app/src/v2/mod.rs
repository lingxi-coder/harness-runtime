//! Schema-2 app library, independent of accounts and conversation lifetime.
//! Filesystem calls are synchronous: async hosts must use their blocking executor.
mod library;
mod manifest;
mod model;
pub use library::*;
pub use manifest::*;
pub use model::*;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid local app: {0}")]
    Invalid(String),
    #[error("local app not found: {0}")]
    NotFound(String),
    #[error("publish conflict: expected revision {expected}, current revision {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("execution consent required for release {0}")]
    ConsentRequired(String),
    #[error("data schema {current} is not compatible with release schema {target}")]
    DataIncompatible { current: u32, target: u32 },
    #[error("data schema change {current} → {target} requires a verified backup and migration transaction; existing data and current release are retained")]
    DataMigrationRequired { current: u32, target: u32 },
    #[error("app is busy: {0}")]
    Busy(String),
    #[error("local app storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("local app JSON: {0}")]
    Json(#[from] serde_json::Error),
}
impl AppError {
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
