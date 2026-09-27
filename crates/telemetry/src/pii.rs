//! PII marker newtypes routing sensitive payloads to the right sink columns.
//!
//! See spec §26.2 / A8 — these are *newtypes*, not type aliases, so a caller
//! must explicitly assert at construction that the inner string is either
//! safe (`Verified`) or destined for a privileged proto column (`PiiTagged`).

use serde::{Deserialize, Serialize};

/// Marker newtype: caller asserts the inner string is NOT code or filepaths.
///
/// Routed to standard (non-proto) `BigQuery` columns and any other general-
/// access sink. Construction is intentionally explicit via
/// [`Verified::assert_safe`] so the assertion is reviewable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verified(String);

impl Verified {
    /// Wrap a string after the caller has confirmed it contains no PII.
    #[must_use]
    pub fn assert_safe(value: String) -> Self {
        Self(value)
    }

    /// Consume the wrapper and return the inner string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Marker newtype: routes to privileged `BigQuery` proto columns via `_PROTO_*` keys.
///
/// Use for values that include code, filepaths, or other potentially-sensitive
/// content. Sinks that don't honour proto-tagged routing MUST strip `_PROTO_`
/// keys before forwarding (see [`strip_proto_fields`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiTagged(String);

impl PiiTagged {
    /// Wrap a string that has been confirmed as destined for a PII-tagged column.
    #[must_use]
    pub fn assert_pii_tagged_column(value: String) -> Self {
        Self(value)
    }

    /// Consume the wrapper and return the inner string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

/// Strip `_PROTO_*` keys from a payload destined for general-access sinks.
///
/// Called by sink adapters that don't route to proto-tagged columns to avoid
/// leaking PII-tagged fields into non-privileged destinations.
pub fn strip_proto_fields(metadata: &mut crate::sink::LogEventMetadata) {
    metadata.retain(|k, _| !k.starts_with("_PROTO_"));
}
