//! The scope of one Local App conversation export, and the grammar of the keys
//! and names derived from it.
//!
//! An app's MCP tools reach a conversation through a logical server whose name
//! and transport registry key are derived from the app and the tool surface
//! last exposed. The scope is bound when the host creates the connection and is
//! never taken from tool input.

use crate::transport::McpError;

/// Scope for a Local App conversation-export connection. The scope is bound
/// when the Host creates the connection; it is never taken from a tool input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationExport {
    /// Stable Local App identity.
    pub app_id: String,
    /// Digest of the tool surface last exposed to the conversation.
    pub listed_tool_surface_sha256: String,
}

impl ConversationExport {
    /// Validate the schema-v3 App ID and the connection's last-listed surface.
    pub fn new(
        app_id: impl Into<String>,
        listed_tool_surface_sha256: impl Into<String>,
    ) -> Result<Self, McpError> {
        let app_id = app_id.into();
        let digest = listed_tool_surface_sha256.into();
        if !is_local_app_id(&app_id) {
            return Err(McpError::Internal("invalid Local App identity".into()));
        }
        if !is_sha256(&digest) {
            return Err(McpError::Internal(
                "invalid Local App tool surface identity".into(),
            ));
        }
        Ok(Self {
            app_id,
            listed_tool_surface_sha256: digest,
        })
    }

    /// Logical MCP server name for this app.
    #[must_use]
    pub fn server_name(&self) -> String {
        format!("local_app_{}", self.app_id)
    }

    /// Registry key for this logical server.
    #[must_use]
    pub fn registry_key(&self) -> String {
        format!("local_apps:conversation-export:{}", self.app_id)
    }

    /// Build the transport registry key for one conversation-scoped export.
    pub fn scoped_registry_key(&self, conversation_id: &str) -> Result<String, McpError> {
        if !is_conversation_scope_id(conversation_id) {
            return Err(McpError::Internal(
                "invalid Local App conversation scope".into(),
            ));
        }
        Ok(format!(
            "local_apps:conversation-export:{conversation_id}:{}:{}",
            self.app_id, self.listed_tool_surface_sha256
        ))
    }

    /// Parse a conversation-scoped Local App transport registry key.
    pub fn parse_scoped_registry_key(
        key: &str,
    ) -> Result<Option<(String, ConversationExport)>, McpError> {
        let Some(rest) = key.strip_prefix("local_apps:conversation-export:") else {
            return Ok(None);
        };
        let mut parts = rest.splitn(3, ':');
        let (Some(conversation_id), Some(app_id), Some(surface)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Ok(None);
        };
        if !is_conversation_scope_id(conversation_id) {
            return Err(McpError::Internal(
                "invalid Local App conversation scope".into(),
            ));
        }
        Ok(Some((
            conversation_id.to_string(),
            Self::new(app_id.to_string(), surface.to_string())?,
        )))
    }

    /// Stable wire identity shared by every logical Local App server.
    #[must_use]
    pub const fn server_info_name(&self) -> &'static str {
        "lingxi-local-app"
    }

    /// Build and validate one model-facing tool name.
    pub fn tool_full_name(&self, tool_name: &str) -> Result<String, McpError> {
        if tool_name.is_empty()
            || tool_name.len() > 64
            || !tool_name.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || (byte == b'_' && index > 0)
            })
            || tool_name.starts_with('_')
            || tool_name.ends_with('_')
            || tool_name.contains("__")
        {
            return Err(McpError::ToolNotFound(tool_name.into()));
        }
        Ok(format!("mcp__{}__{}", self.server_name(), tool_name))
    }
}

/// Whether `value` is a well-formed Local App id: 1 to 54 lowercase letters, digits or
/// hyphens, not starting with a hyphen.
pub fn is_local_app_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 54
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Whether `value` is a lowercase hex SHA-256 digest.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether `value` is a usable conversation scope id: 1 to 128 ASCII letters, digits,
/// hyphens or underscores.
pub fn is_conversation_scope_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Whether `value` is a well-formed raw tool name: lowercase snake case of at most 64
/// characters, without a leading, trailing or doubled underscore.
pub fn is_local_app_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (byte == b'_' && index > 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The names below are persisted in saved conversations and approvals and spoken by the model; they moved
    // here from the engine's registry tests, which no longer know what the servers are.
    #[test]
    fn conversation_export_identity_preserves_hyphens_and_split_boundaries() {
        let scope = ConversationExport::new("abc--1", "0".repeat(64)).unwrap();
        assert_eq!(scope.server_name(), "local_app_abc--1");
        assert_eq!(scope.server_info_name(), "lingxi-local-app");
        assert_eq!(
            scope.registry_key(),
            "local_apps:conversation-export:abc--1"
        );
        assert_eq!(
            scope.tool_full_name("read_value").unwrap(),
            "mcp__local_app_abc--1__read_value"
        );
        assert!(ConversationExport::new("abc_1", "0".repeat(64)).is_err());
        assert!(scope.tool_full_name("bad__name").is_err());
    }

    #[test]
    fn conversation_export_uses_schema_v3_app_id_boundaries() {
        let id_54 = format!("a{}", "b".repeat(53));
        let id_55 = format!("a{}", "b".repeat(54));
        assert!(ConversationExport::new(id_54, "0".repeat(64)).is_ok());
        assert!(ConversationExport::new(id_55, "0".repeat(64)).is_err());
        assert!(ConversationExport::new("A123", "0".repeat(64)).is_err());
        assert!(ConversationExport::new("-leading", "0".repeat(64)).is_err());
    }
}
