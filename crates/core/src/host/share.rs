//! `SharingService` — native share-sheet seam (M8-P10).
//!
//! Implemented natively in Swift/Kotlin via `UniFFI` (P12) and injected into the
//! mobile `Platform`. The `tool-share` tool (P11) routes through it.

use async_trait::async_trait;
use thiserror::Error;

/// Content to hand to the native share sheet. Any combination of fields may be
/// set; at least one should be non-empty.
#[derive(Debug, Clone, Default)]
pub struct SharePayload {
    /// Plain-text body.
    pub text: Option<String>,
    /// An image to share, encoded (PNG/JPEG).
    pub image_bytes: Option<Vec<u8>>,
    /// A URL to share.
    pub url: Option<String>,
}

/// Outcome of presenting the share sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareResult {
    /// The user completed a share action.
    Success,
    /// The user dismissed the sheet.
    Cancelled,
}

/// Failure modes for [`SharingService`] operations.
#[derive(Debug, Clone, Error)]
pub enum ShareError {
    /// Sharing is not supported on this platform.
    #[error("sharing unsupported")]
    Unsupported,
    /// Any other native failure.
    #[error("share error: {0}")]
    Other(String),
}

/// Native share-sheet presentation.
#[async_trait]
pub trait SharingService: Send + Sync {
    /// Present the native share sheet with `payload`.
    async fn share(&self, payload: SharePayload) -> Result<ShareResult, ShareError>;
}
