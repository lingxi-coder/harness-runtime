//! Newtype-wrapped identifiers used across the engine.
//!
//! Each ID is a UUID v4 internally but serializes as a plain string so the
//! protocol stays language-neutral across the `UniFFI` bridge.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident, $prefix:literal) => {
        #[doc = concat!("Identifier for a ", stringify!($name), ". UUID v4 internally; ")]
        #[doc = concat!("serialized with the `", $prefix, ":` prefix for log-grep-ability.")]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
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
            /// robustness. Used to recover an id surfaced as a plain string in a
            /// tool result (e.g. the Agent tool's `data.agentId`).
            #[must_use]
            pub fn parse_prefixed(s: impl AsRef<str>) -> Option<Self> {
                let s = s.as_ref();
                let body = s.strip_prefix(concat!($prefix, ":")).unwrap_or(s);
                Uuid::parse_str(body).ok().map(Self)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", $prefix, self.0)
            }
        }
    };
}

id_newtype!(AgentId, "agent");
id_newtype!(SessionId, "sess");
id_newtype!(MessageId, "msg");
id_newtype!(RequestId, "req");
id_newtype!(HookId, "hook");
id_newtype!(PluginId, "plg");
id_newtype!(McpConnectionId, "mcp");
id_newtype!(SnapshotId, "snap");
id_newtype!(PrefetchId, "pf");

/// Identifier for a tool-use / tool-call.
///
/// Unlike the other id types, `ToolUseId` is a **String-backed** newtype that
/// holds the *canonical* provider-issued id (e.g. `toolu_01ABC…`, `call_…`)
/// verbatim, so resume/JSONL bytes match upstream `claude-code`. It serializes
/// transparently as a bare JSON string. Internally-minted ids (when no provider
/// string is available) are synthesized as `toolu_<uuid>` so they stay unique
/// and look plausible. It is intentionally NOT `Copy` (it owns a `String`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolUseId(String);

impl ToolUseId {
    /// Generate a fresh synthetic id. Used when no provider id is available;
    /// looks like a provider `toolu_…` id and is globally unique.
    #[must_use]
    pub fn new() -> Self {
        Self(format!("toolu_{}", Uuid::new_v4().simple()))
    }

    /// Borrow the inner canonical id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ToolUseId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<String> for ToolUseId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ToolUseId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Display for ToolUseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_new_is_unique() {
        let a = AgentId::new();
        let b = AgentId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn agent_id_roundtrip_json() {
        let a = AgentId::new();
        let s = serde_json::to_string(&a).unwrap();
        let b: AgentId = serde_json::from_str(&s).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn agent_id_display_prefixed() {
        let a = AgentId::from_uuid(Uuid::nil());
        assert_eq!(format!("{a}"), "agent:00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn session_id_distinct_type_from_agent_id() {
        // This test exists only to lock in type discipline; the assertion is trivial.
        let _: SessionId = SessionId::new();
        let _: AgentId = AgentId::new();
    }
}
