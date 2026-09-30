//! Non-sensitive device status exposed to app-scoped mobile capabilities.

use async_trait::async_trait;
use thiserror::Error;

/// A bounded snapshot of device state safe to expose to a local app.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    /// Battery percentage in the range 0..=100, when the platform reports it.
    pub battery_percent: Option<f32>,
    /// Whether the device is currently charging, when known.
    pub charging: Option<bool>,
    /// Coarse connectivity label such as `wifi`, `cellular`, `offline`, or
    /// `unknown`; no network identifiers are included.
    pub network: String,
    /// Whether the platform is in a low-power mode, when known.
    pub low_power_mode: Option<bool>,
}

/// Failure modes for a device-status query.
#[derive(Debug, Clone, Error)]
pub enum DeviceStatusError {
    /// The platform does not expose a status snapshot.
    #[error("device status unavailable")]
    Unavailable,
    /// Any other native failure.
    #[error("device status error: {0}")]
    Other(String),
}

/// Native provider for a non-sensitive device-status snapshot.
#[async_trait]
pub trait DeviceStatusProvider: Send + Sync {
    /// Return the latest bounded status snapshot.
    async fn status(&self) -> Result<DeviceStatus, DeviceStatusError>;
}
