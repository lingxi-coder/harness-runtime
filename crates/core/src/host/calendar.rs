//! Bounded read-only calendar access for mobile platforms.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A bounded calendar query expressed in epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarQuery {
    /// Inclusive lower bound in epoch milliseconds.
    pub start_ms: u64,
    /// Exclusive upper bound in epoch milliseconds.
    pub end_ms: u64,
    /// Maximum number of events to return.
    pub limit: u32,
}

/// The small calendar projection exposed to local apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Stable native event identifier.
    pub id: String,
    /// User-visible event title.
    pub title: String,
    /// Event start in epoch milliseconds.
    pub start_ms: u64,
    /// Event end in epoch milliseconds.
    pub end_ms: u64,
    /// Whether the event occupies a whole day.
    pub all_day: bool,
    /// Optional event location.
    pub location: Option<String>,
    /// Optional event notes.
    pub notes: Option<String>,
    /// Optional source calendar name.
    pub calendar: Option<String>,
}

/// Calendar failures mapped at the local-app bridge boundary.
#[derive(Debug, Clone, Error)]
pub enum CalendarError {
    /// The platform has no calendar provider.
    #[error("calendar unavailable")]
    Unavailable,
    /// The user denied calendar access.
    #[error("calendar permission denied")]
    PermissionDenied,
    /// The request cannot be served by the provider.
    #[error("invalid calendar request: {0}")]
    Invalid(String),
    /// An unexpected provider failure.
    #[error("calendar error: {0}")]
    Other(String),
}

/// Native, read-only calendar provider.
#[async_trait]
pub trait CalendarProvider: Send + Sync {
    /// Return the bounded events intersecting a time range.
    async fn list_events(&self, query: CalendarQuery) -> Result<Vec<CalendarEvent>, CalendarError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_query_serializes_snake_case_for_native_bridges() {
        let json = serde_json::to_value(&CalendarQuery {
            start_ms: 1_000,
            end_ms: 2_000,
            limit: 8,
        })
        .expect("serialize calendar query");
        assert_eq!(json["start_ms"], 1_000);
        assert_eq!(json["end_ms"], 2_000);
        assert_eq!(json["limit"], 8);
        assert!(json.get("startMs").is_none());
        assert!(json.get("endMs").is_none());
    }
}
