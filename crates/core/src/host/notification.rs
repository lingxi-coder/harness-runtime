//! `NotificationService` — system-notification seam (mobile device capability).
//!
//! Implemented natively in Swift/Kotlin via `UniFFI` and injected into the
//! mobile `Platform`. The `tool-notification` tool routes through it so the
//! model can post a local notification (the engine-driven analog of the
//! camera/voice/share seams) without knowing the native notification API. On
//! Android the impl wraps `NotificationManager`; on iOS, `UNUserNotificationCenter`.
//!
//! Modeled as `Option` on the `Platform` (like `stt`/`tts`), so no stub is
//! needed on desktop or non-notification platforms.

use async_trait::async_trait;
use thiserror::Error;

/// A request to post a single system notification.
#[derive(Debug, Clone)]
pub struct NotificationRequest {
    /// Notification title (the bold first line).
    pub title: String,
    /// Notification body text.
    pub body: String,
    /// Optional channel/identifier tag — lets a later post replace an earlier
    /// one (Android channel id / iOS request identifier). `None` = a fresh,
    /// non-coalescing notification.
    pub tag: Option<String>,
}

/// Failure modes for [`NotificationService`] operations.
#[derive(Debug, Clone, Error)]
pub enum NotificationError {
    /// The user denied notification permission.
    #[error("notification permission denied")]
    PermissionDenied,
    /// Any other native failure.
    #[error("notification error: {0}")]
    Other(String),
}

/// Native system notifications (engine-driven local notification post).
#[async_trait]
pub trait NotificationService: Send + Sync {
    /// Post a single local notification. Implementations honor the platform's
    /// permission model (a denied prompt surfaces [`NotificationError::PermissionDenied`]).
    async fn notify(&self, req: NotificationRequest) -> Result<(), NotificationError>;
}
