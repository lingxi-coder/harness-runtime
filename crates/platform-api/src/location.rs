//! `LocationProvider` — one-shot device location seam.
//!
//! Implemented natively in Swift/Kotlin via `UniFFI` and injected into the
//! mobile `Platform`. The local-apps device bridge routes through it so Rust
//! can request a fix without knowing the native location API. One-shot only:
//! continuous tracking is deliberately absent until a host→page push channel
//! exists to deliver it.

use async_trait::async_trait;
use thiserror::Error;

/// A single resolved device location.
#[derive(Debug, Clone)]
pub struct LocationFix {
    /// Latitude in decimal degrees (WGS-84).
    pub latitude: f64,
    /// Longitude in decimal degrees (WGS-84).
    pub longitude: f64,
    /// Horizontal accuracy radius in meters, when the platform reports one.
    pub accuracy_m: Option<f64>,
    /// Fix timestamp, epoch milliseconds.
    pub timestamp_ms: u64,
}

/// Failure modes for [`LocationProvider`] operations.
#[derive(Debug, Clone, Error)]
pub enum LocationError {
    /// The user denied location permission.
    #[error("location permission denied")]
    PermissionDenied,
    /// Location services are unavailable (disabled, no hardware, restricted).
    #[error("location unavailable")]
    Unavailable,
    /// No fix arrived within the native layer's own deadline.
    #[error("location timed out")]
    Timeout,
    /// Any other native failure.
    #[error("location error: {0}")]
    Other(String),
}

/// Native one-shot location access.
#[async_trait]
pub trait LocationProvider: Send + Sync {
    /// Resolve the device's current location once.
    async fn current_location(&self) -> Result<LocationFix, LocationError>;
}
