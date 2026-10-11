//! External deep-link opening seam for mobile platforms.

use async_trait::async_trait;
use thiserror::Error;

/// Failure modes for opening an external URL through the host UI.
#[derive(Debug, Clone, Error)]
pub enum DeepLinkError {
    /// The platform cannot open the requested link.
    #[error("deep link unavailable")]
    Unavailable,
    /// The platform rejected the link.
    #[error("deep link rejected: {0}")]
    Rejected(String),
    /// Any other native failure.
    #[error("deep link error: {0}")]
    Other(String),
}

/// Native opener for an explicitly authorized external deep link.
#[async_trait]
pub trait DeepLinkOpener: Send + Sync {
    /// Ask the platform to open one already-validated URL.
    async fn open(&self, url: String) -> Result<(), DeepLinkError>;
}
