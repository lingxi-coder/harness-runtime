//! Domain-typed labels for secrets in storage.
//!
//! [`SecretKind`] is the typed in-engine label; [`SecretKindDto`] (in
//! `lingxi-protocol`) is its serialized form attached to
//! `SecureStorageMetadata`. The pair avoids a circular dependency between the
//! protocol crate and this crate.

use lingxi_core::types::SecretKindDto;
use serde::{Deserialize, Serialize};

/// Typed label for every secret persisted via `SecureStorage`.
///
/// Variants distinguish Anthropic-owned secrets, MCP server tokens scoped by
/// server identifier, and generic API keys scoped by provider name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretKind {
    /// Anthropic-issued API key (`sk-ant-api03-...`).
    AnthropicApiKey,
    /// Short-lived Anthropic OAuth access token.
    AnthropicOAuthAccessToken,
    /// Long-lived Anthropic OAuth refresh token.
    AnthropicOAuthRefreshToken,
    /// Non-secret Anthropic OAuth session metadata (email / org / expiry /
    /// scopes). Distinct from the access/refresh tokens so a keychain audit or
    /// redaction-by-kind pass does not miscount it as token material.
    AnthropicOAuthSessionMeta,
    /// AWS credentials (access key + secret pair).
    AwsCredentials,
    /// OAuth access token for an MCP server identified by `server`.
    McpOAuthAccessToken {
        /// Stable identifier for the MCP server that issued this token.
        server: String,
    },
    /// OAuth refresh token for an MCP server identified by `server`.
    McpOAuthRefreshToken {
        /// Stable identifier for the MCP server that issued this token.
        server: String,
    },
    /// Generic API key scoped by an upstream provider name.
    GenericApiKey {
        /// Stable identifier for the third-party provider.
        provider: String,
    },
    /// A sensitive plugin `userConfig` value, scoped by the owning `plugin`
    /// identity and the field `key`. Mirrors claude-code's `pluginSecrets`
    /// keychain entries (keyed `${plugin}/${key}`), which back a plugin's
    /// `sensitive: true` userConfig fields (never persisted to settings.json).
    PluginSecret {
        /// Owning plugin identity (`name` or `name@marketplace`).
        plugin: String,
        /// userConfig field key.
        key: String,
    },
    /// Short-lived `OpenAI` / `ChatGPT` OAuth access token.
    OpenAiOAuthAccessToken,
    /// Long-lived `OpenAI` / `ChatGPT` OAuth refresh token.
    OpenAiOAuthRefreshToken,
    /// Non-secret `OpenAI` OAuth session metadata (`account_id` / fedramp / expiry /
    /// scopes). Distinct from the access/refresh tokens so a keychain audit or
    /// redaction-by-kind pass does not miscount it as token material.
    OpenAiOAuthSessionMeta,
}

impl SecretKind {
    /// Project this `SecretKind` into its DTO form for inclusion in
    /// `SecureStorageMetadata`.
    ///
    /// Serialization failures (which should not occur for the variants in this
    /// enum) collapse to the sentinel string `<invalid>` rather than panicking.
    #[must_use]
    pub fn as_dto(&self) -> SecretKindDto {
        SecretKindDto(serde_json::to_string(self).unwrap_or_else(|_| "<invalid>".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_api_key_round_trips_via_dto() {
        let dto = SecretKind::AnthropicApiKey.as_dto();
        let parsed: SecretKind = serde_json::from_str(&dto.0).expect("dto string is JSON");
        assert_eq!(parsed, SecretKind::AnthropicApiKey);
    }

    #[test]
    fn mcp_token_carries_server_id() {
        let kind = SecretKind::McpOAuthAccessToken {
            server: "files.local".into(),
        };
        let dto = kind.as_dto();
        assert!(dto.0.contains("files.local"));
    }
}
