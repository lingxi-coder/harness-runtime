//! Reply shape from `EffectHandler` back to the run loop.

use crate::ids::{RequestId, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Successful reply from an `EffectHandler` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectResult {
    /// No-op (`Render*`, `RecordUnexpectedEvent`, etc.).
    Ack,
    /// API request was accepted; events will follow asynchronously.
    ApiRequestQueued {
        /// Correlation ID of the queued request.
        request_id: RequestId,
    },
    /// Session loaded successfully.
    SessionLoaded {
        /// Identifier of the loaded session.
        session_id: SessionId,
    },
}

/// Failure reply from an `EffectHandler` invocation.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[error("effect failed: {kind}: {detail}")]
pub struct EffectError {
    /// Coarse category of the failure.
    pub kind: EffectErrorKind,
    /// Human-readable detail string for logs and telemetry.
    pub detail: String,
}

/// Coarse failure category for an `EffectError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectErrorKind {
    /// Local I/O failure (disk, pipes).
    Io,
    /// Network or transport failure.
    Network,
    /// Permission / authorization denied by the host.
    Permission,
    /// Requested resource was not found.
    NotFound,
    /// Operation was cancelled before completion.
    Cancelled,
    /// Catch-all for unexpected internal errors.
    Internal,
}

impl std::fmt::Display for EffectErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Io => "io",
            Self::Network => "network",
            Self::Permission => "permission",
            Self::NotFound => "not_found",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        };
        f.write_str(s)
    }
}
