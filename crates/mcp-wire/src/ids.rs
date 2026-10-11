//! The id of one live MCP connection.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Identifier for one MCP connection. UUID v4 internally; displayed with the
/// `mcp:` prefix for log-grep-ability and serialized as the bare UUID string.
///
/// The engine's id types are all defined the same way (`core::types`), and this
/// one is the same type as the engine's `McpConnectionId`: it moved here so a
/// transport can be written without depending on the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpConnectionId(Uuid);

impl McpConnectionId {
    /// Generate a fresh random ID.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Nil ID (all zeros) — for sentinel values, not for production.
    #[must_use]
    pub fn nil() -> Self {
        Self(Uuid::nil())
    }

    /// Construct from a raw UUID. Useful for tests and deserialization fallbacks.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Return the underlying UUID. Useful when interoperating with
    /// libraries that take `uuid::Uuid` directly.
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// Parse the prefixed display form (`"<prefix>:<uuid>"`, as produced
    /// by [`fmt::Display`]) back into the id, returning `None` on a
    /// malformed string. A bare `<uuid>` (no prefix) is also accepted for
    /// robustness.
    #[must_use]
    pub fn parse_prefixed(s: impl AsRef<str>) -> Option<Self> {
        let s = s.as_ref();
        let body = s.strip_prefix("mcp:").unwrap_or(s);
        Uuid::parse_str(body).ok().map(Self)
    }
}

impl Default for McpConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for McpConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mcp:{}", self.0)
    }
}
