//! Telemetry error type for schema validation and sink dispatch failures.
//!
//! See spec §5 lines 502-510. The two variants here are the M3-locked surface;
//! future variants must be `#[non_exhaustive]`-friendly additions (the enum is
//! marked `#[non_exhaustive]` so adding a variant is non-breaking).

use thiserror::Error;

/// Errors raised by the telemetry subsystem during schema validation, sink
/// dispatch, or proc-macro audit.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum TelemetryError {
    /// The event name is not in the `tengu_*` schema registry.
    ///
    /// Display string locked byte-for-byte against spec §5 line 506-507:
    /// `"unknown event name {name} (not in tengu_* schema)"`.
    #[error("unknown event name {name} (not in tengu_* schema)")]
    UnknownEvent {
        /// The event name that was rejected.
        name: String,
    },
    /// The event payload failed schema validation.
    ///
    /// Display string locked byte-for-byte against spec §5 line 508-509:
    /// `"event payload validation failed for {event}: {detail}"`.
    #[error("event payload validation failed for {event}: {detail}")]
    PayloadInvalid {
        /// The event name whose payload was invalid.
        event: String,
        /// Human-readable detail; safe to log (no PII per discipline).
        detail: String,
    },
}
