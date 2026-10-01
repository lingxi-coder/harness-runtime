//! `mcp-wire` — MCP as data and as an interface: the `Tool` wire shapes and the
//! per-tool permission ceiling, the rest of the wire DTOs and the
//! [`transport::McpTransport`] trait, the connection id, and the scope of a
//! Local App conversation export ([`export`]).
//!
//! The types describe a tool, a resource or a result the way the MCP
//! specification does (`name`, `inputSchema`, `annotations`, `icons`, `_meta`,
//! ...) and the tighten-only ceiling a host puts on one. They hold no behaviour
//! beyond parsing and ordering, and nothing here depends on the engine, so the
//! engine (`lingxi-core` re-exports them) and the Local App service can both
//! name them, and the service can implement a transport, without either
//! depending on the other.

#![forbid(unsafe_code)]

pub mod export;
mod ids;
pub mod transport;

pub use ids::McpConnectionId;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tighten-only per-tool ceiling supplied by host/org policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpPermissionCeiling {
    /// No additional restriction.
    Allow,
    /// Require interactive approval at minimum.
    Ask,
    /// Block the tool.
    Deny,
}

impl McpPermissionCeiling {
    /// Parse `toolPermissions`/org-policy values (`blocked` maps to `deny`).
    #[must_use]
    pub fn from_policy_str(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "blocked" | "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// Return the stricter of two ceilings (`deny` > `ask` > `allow`).
    #[must_use]
    pub const fn strictest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Ask, _) | (_, Self::Ask) => Self::Ask,
            (Self::Allow, Self::Allow) => Self::Allow,
        }
    }
}

/// Standard MCP icon descriptor for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpIconDto {
    /// Verified icon source URI.
    pub src: String,
    /// Optional MIME type for the icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Declared size labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<String>,
    /// Optional theme discriminator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Unrecognized extension members preserved across transport/cache hops.
    #[serde(flatten, default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub extra: indexmap::IndexMap<String, Value>,
}

/// Optional MCP tool annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotationsDto {
    /// Optional user-facing title override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the tool is host-proven read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Whether the tool may be destructive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Whether the host can prove the tool is idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Whether the tool touches the outside world.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
    /// Unrecognized annotation hints preserved across transport/cache hops.
    #[serde(flatten, default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub extra: indexmap::IndexMap<String, Value>,
}

/// MCP task-support declaration for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolTaskSupportDto {
    /// This tool does not participate in MCP Tasks.
    Forbidden,
    /// This tool may opt into MCP Tasks.
    Optional,
    /// This tool requires MCP Tasks.
    Required,
}

/// Optional execution metadata for one tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolExecutionDto {
    /// Declared MCP Tasks support level for this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_support: Option<McpToolTaskSupportDto>,
}

/// Standard MCP 2025-11-25 `Tool` wire definition used by Local App catalogs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinitionDto {
    /// Raw tool name exposed by the server.
    pub name: String,
    /// Optional user-facing title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional user-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Input JSON Schema.
    pub input_schema: Value,
    /// Optional structured output JSON Schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Optional derived annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotationsDto>,
    /// Optional execution metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<McpToolExecutionDto>,
    /// Optional icon list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<McpIconDto>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl McpToolDefinitionDto {
    /// Construct a minimal wire `Tool` definition.
    #[must_use]
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            input_schema,
            output_schema: None,
            annotations: None,
            execution: None,
            icons: Vec::new(),
            meta: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ceiling_is_tighten_only() {
        use McpPermissionCeiling::{Allow, Ask, Deny};
        for (a, b, strictest) in [
            (Allow, Allow, Allow),
            (Allow, Ask, Ask),
            (Allow, Deny, Deny),
            (Ask, Ask, Ask),
            (Ask, Deny, Deny),
            (Deny, Deny, Deny),
        ] {
            assert_eq!(a.strictest(b), strictest, "{a:?} then {b:?}");
            assert_eq!(b.strictest(a), strictest, "{b:?} then {a:?}");
        }
    }

    #[test]
    fn the_ceiling_parses_policy_strings_and_names_itself_in_lowercase() {
        assert_eq!(
            McpPermissionCeiling::from_policy_str("allow"),
            Some(McpPermissionCeiling::Allow)
        );
        assert_eq!(
            McpPermissionCeiling::from_policy_str("ask"),
            Some(McpPermissionCeiling::Ask)
        );
        // Org policy says `blocked`; the wire says `deny`. Both mean the same.
        assert_eq!(
            McpPermissionCeiling::from_policy_str("blocked"),
            Some(McpPermissionCeiling::Deny)
        );
        assert_eq!(
            McpPermissionCeiling::from_policy_str("deny"),
            Some(McpPermissionCeiling::Deny)
        );
        assert_eq!(McpPermissionCeiling::from_policy_str("Allow"), None);
        assert_eq!(
            serde_json::to_value(McpPermissionCeiling::Deny).unwrap(),
            json!("deny")
        );
    }

    /// A catalog digest hashes this serialization, so a field that starts or
    /// stops appearing for a minimal tool would invalidate every approval.
    #[test]
    fn a_minimal_tool_serializes_only_its_required_fields() {
        let tool = McpToolDefinitionDto::new("runtime_status", json!({"type": "object"}));
        assert_eq!(
            serde_json::to_value(&tool).unwrap(),
            json!({"name": "runtime_status", "inputSchema": {"type": "object"}})
        );
    }

    #[test]
    fn a_full_tool_keeps_its_wire_names_and_every_unknown_member() {
        let wire = json!({
            "name": "list_notes",
            "title": "List notes",
            "description": "Lists the notes.",
            "inputSchema": {"type": "object", "properties": {}},
            "outputSchema": {"type": "object"},
            "annotations": {
                "title": "Notes",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false,
                "x-vendor-hint": {"keep": "me"}
            },
            "execution": {"taskSupport": "optional"},
            "icons": [{
                "src": "https://example.test/icon.png",
                "mimeType": "image/png",
                "sizes": ["48x48"],
                "theme": "dark",
                "x-vendor-icon": 1
            }],
            "_meta": {"ui": {"resourceUri": "ui://notes"}}
        });
        let tool: McpToolDefinitionDto = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            tool.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        assert_eq!(
            tool.execution.as_ref().unwrap().task_support,
            Some(McpToolTaskSupportDto::Optional)
        );
        assert_eq!(tool.icons[0].mime_type.as_deref(), Some("image/png"));
        assert_eq!(serde_json::to_value(&tool).unwrap(), wire);
    }
}
