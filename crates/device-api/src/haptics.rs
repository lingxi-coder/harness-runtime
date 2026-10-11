//! Bounded haptic feedback seam for mobile platforms.

use async_trait::async_trait;
use thiserror::Error;

/// Host-approved haptic styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HapticStyle {
    /// A light tap.
    Light,
    /// A medium tap.
    Medium,
    /// A heavy tap.
    Heavy,
    /// A success notification.
    Success,
    /// A warning notification.
    Warning,
    /// An error notification.
    Error,
}

/// Failure modes for haptic feedback.
#[derive(Debug, Clone, Error)]
pub enum HapticError {
    /// The platform does not support haptics.
    #[error("haptics unavailable")]
    Unavailable,
    /// Any other native failure.
    #[error("haptic error: {0}")]
    Other(String),
}

/// Native bounded haptic feedback.
#[async_trait]
pub trait HapticService: Send + Sync {
    /// Trigger one bounded feedback event.
    async fn trigger(&self, style: HapticStyle) -> Result<(), HapticError>;
}
