//! Host-owned MCP provenance exposed to tool hooks.

use crate::{ConfigScope, McpServerConfig};
use lingxi_core::host::McpTransportSpec;
use lingxi_core::types::SettingsScope;
use serde::{Deserialize, Serialize};

/// MCP server identity attached to a tool hook. This is derived from the
/// configured host record, never from a model-visible normalized alias.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerProvenance {
    /// Logical server name before MCP normalization.
    pub name: String,
    /// Native provenance source (`user`, `project`, `plugin`, `sdk`, etc.).
    pub source: String,
}

impl McpServerProvenance {
    /// Resolve the 2.1.322 provenance precedence: explicit host source (which
    /// includes trusted plugin ownership), actual SDK transport, then scope.
    ///
    /// Native's dynamic-mod branch checks its module-owner registry. Hosts
    /// should stamp `metadata.provenance_source` after proving ownership; a
    /// server name merely beginning with `plugin:` is not such proof.
    #[must_use]
    pub fn from_config(config: &McpServerConfig) -> Self {
        let source = config
            .metadata
            .provenance_source
            .as_deref()
            .unwrap_or_else(|| {
                if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
                    "sdk"
                } else {
                    match config.scope {
                        ConfigScope::Settings(SettingsScope::Local) => "local",
                        ConfigScope::Settings(SettingsScope::User) => "user",
                        ConfigScope::Settings(SettingsScope::Project) => "project",
                        ConfigScope::Settings(SettingsScope::Managed) => "managed",
                        ConfigScope::Dynamic => "dynamic",
                        ConfigScope::Enterprise => "enterprise",
                        ConfigScope::ClaudeAi => "claudeai",
                        ConfigScope::Agent => "agent",
                    }
                }
            });
        Self {
            name: config.name.clone(),
            source: source.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_server_from_json_entry;
    use serde_json::json;

    #[test]
    fn provenance_322_matches_native_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_2_1_322.json"))
                .expect("native MCP fixture");
        for case in fixture["provenanceCases"].as_array().expect("cases") {
            let input = &case["input"];
            let scope = match input["scope"].as_str().unwrap_or("dynamic") {
                "local" => ConfigScope::Settings(SettingsScope::Local),
                "user" => ConfigScope::Settings(SettingsScope::User),
                "project" => ConfigScope::Settings(SettingsScope::Project),
                "managed" => ConfigScope::Settings(SettingsScope::Managed),
                "enterprise" => ConfigScope::Enterprise,
                "claudeai" => ConfigScope::ClaudeAi,
                "agent" => ConfigScope::Agent,
                "dynamic" => ConfigScope::Dynamic,
                other => panic!("unsupported fixture scope {other}"),
            };
            let entry = if input["serverType"] == "sdk" {
                json!({"type":"sdk","name":"wire-sdk"})
            } else {
                json!({"command":"server"})
            };
            let mut config = build_server_from_json_entry(
                input["serverName"].as_str().expect("server name"),
                &entry,
                scope,
            )
            .expect("valid config");
            config.metadata.provenance_source = input["source"].as_str().map(str::to_owned);
            if input.get("pluginTelemetry").is_some() && config.metadata.provenance_source.is_none()
            {
                // The host plugin loader proves ownership; agentSource alone
                // only identifies the agent definition's origin.
                config.metadata.provenance_source = Some("plugin".into());
            }
            if input["ownedByMod"] == true
                && scope == ConfigScope::Dynamic
                && config.metadata.provenance_source.is_none()
            {
                config.metadata.provenance_source = Some("plugin".into());
            }
            assert_eq!(
                serde_json::to_value(McpServerProvenance::from_config(&config)).unwrap(),
                case["expected"],
                "{input}",
            );
        }
    }

    #[test]
    fn plugin_authored_agent_inline_server_keeps_agent_provenance() {
        let mut config = build_server_from_json_entry(
            "inline",
            &json!({"command":"server"}),
            ConfigScope::Agent,
        )
        .unwrap();
        config.metadata.agent_source = Some(crate::McpAgentSource::Plugin);
        assert_eq!(McpServerProvenance::from_config(&config).source, "agent");
        config.metadata.provenance_source = Some("plugin".into());
        assert_eq!(McpServerProvenance::from_config(&config).source, "plugin");
    }

    #[test]
    fn plugin_looking_name_does_not_manufacture_ownership() {
        let config = build_server_from_json_entry(
            "plugin:spoof:srv",
            &json!({"command":"server"}),
            ConfigScope::Dynamic,
        )
        .unwrap();
        assert_eq!(McpServerProvenance::from_config(&config).source, "dynamic");
    }

    #[test]
    fn ordinary_json_cannot_supply_host_provenance() {
        let config = build_server_from_json_entry(
            "sdk",
            &json!({"command":"server","source":"sdk","provenance_source":"plugin"}),
            ConfigScope::Settings(SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(McpServerProvenance::from_config(&config).source, "project");
    }
}
