//! Host-derived MCP capability identities. Model-facing aliases never authorize a call.

use crate::McpServerConfig;
use lingxi_core::host::McpToolDto;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One exact tool definition from one exact configured server revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpToolIdentity {
    /// Stable host configuration slot; independent of MCP identifier normalization.
    pub server_config_id: String,
    /// Digest of the actual host configuration, including transport and policy.
    pub config_revision: String,
    /// Original server tools/list name, before any alias normalization.
    pub raw_tool_name: String,
    /// Digest of the exact discovered definition presented for authorization.
    pub definition_fingerprint: String,
}

fn fingerprint(value: &impl Serialize) -> String {
    // These DTOs contain only JSON-serializable source values. Failure is fatal:
    // substituting an empty/fallback digest could merge unrelated capabilities.
    let bytes = serde_json::to_vec(value).expect("MCP identity source must serialize");
    format!("{:x}", Sha256::digest(bytes))
}

fn config_fingerprint(config: &McpServerConfig) -> String {
    // Configuration maps (notably stdio env) may be HashMaps whose iteration
    // order changes across processes. Sort object keys before hashing so the
    // trusted host catalog and app-agent child agree on the same actual config.
    fn canonical(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let sorted: std::collections::BTreeMap<_, _> = map
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(canonical).collect())
            }
            other => other,
        }
    }
    fingerprint(&canonical(
        serde_json::to_value(config).expect("MCP config must serialize"),
    ))
}

impl McpToolIdentity {
    /// Mint from trusted loaded configuration and the actual discovery result.
    /// Never accept an identity reconstructed from a display FQN.
    #[must_use]
    pub fn from_config_tool(config: &McpServerConfig, tool: &McpToolDto) -> Self {
        Self {
            server_config_id: fingerprint(&(&config.scope, &config.name)),
            config_revision: config_fingerprint(config),
            raw_tool_name: tool.tool_name.clone(),
            definition_fingerprint: fingerprint(tool),
        }
    }
}

/// Host capability guard called again at every actual RPC dispatch, including
/// after lazy connection establishment and authentication/session retries.
pub type McpToolDispatchGuard = std::sync::Arc<dyn Fn(&McpToolIdentity) -> bool + Send + Sync>;
