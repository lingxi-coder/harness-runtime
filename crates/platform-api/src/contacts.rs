//! Bounded read-only contacts access for mobile platforms.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A bounded contacts search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactsQuery {
    /// Trimmed display-name search text.
    pub query: String,
    /// Maximum number of contacts to return.
    pub limit: u32,
}

/// The deliberately small contact projection exposed to local apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// Stable native contact identifier.
    pub id: String,
    /// User-visible contact name.
    pub display_name: String,
    /// Bounded phone projections.
    pub phones: Vec<String>,
    /// Bounded email projections.
    pub emails: Vec<String>,
}

/// Contacts failures mapped at the local-app bridge boundary.
#[derive(Debug, Clone, Error)]
pub enum ContactsError {
    /// The platform has no contacts provider.
    #[error("contacts unavailable")]
    Unavailable,
    /// The user denied contacts access.
    #[error("contacts permission denied")]
    PermissionDenied,
    /// The request cannot be served by the provider.
    #[error("invalid contacts request: {0}")]
    Invalid(String),
    /// An unexpected provider failure.
    #[error("contacts error: {0}")]
    Other(String),
}

/// Native, read-only contacts provider.
#[async_trait]
pub trait ContactsProvider: Send + Sync {
    /// Search the bounded contact projection.
    async fn search(&self, query: ContactsQuery) -> Result<Vec<Contact>, ContactsError>;
}
