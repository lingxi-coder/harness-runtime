//! Scope and startup gates for Native MCP `permission_policy` rules.
//!
//! These declarations are distinct from organization `toolPermissions` and
//! `org_max_permission` ceilings. Ordinary local HTTP/SSE/stdio configuration
//! must not silently turn them into unconditional tool metadata restrictions.

use crate::{ConfigScope, McpServerConfig};
use lingxi_core::host::permission_gate::{
    PermissionGate, PermissionToolCheckBehavior, PermissionToolCheckRule,
    PermissionToolCheckRuleSource,
};
use lingxi_core::host::{McpConfiguredToolPolicyDto, McpToolPermissionPolicy, McpTransportSpec};

pub(crate) type SdkInitializeRules = std::collections::HashMap<
    String,
    std::collections::BTreeMap<String, PermissionToolCheckBehavior>,
>;

/// Publish the complete initialize policy batch before acknowledging it or
/// awaiting any transport. No lifecycle lock or handshake is needed here.
pub async fn apply_sdk_initialize_policies(
    registry: &crate::McpRegistry,
    configs: &[McpServerConfig],
    gate: &dyn PermissionGate,
) {
    let _publication = registry.permission_policy_publish_lock.lock().await;
    let connections = registry.connections.read().await.clone();
    {
        let mut declared = registry
            .sdk_initialize_permission_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for config in configs {
            if !matches!(config.spec, McpTransportSpec::SdkControl { .. })
                || connections.get(&config.name).is_some_and(|state| {
                    !matches!(state.config().spec, McpTransportSpec::SdkControl { .. })
                })
            {
                continue;
            }
            let rules = declared.entry(config.name.clone()).or_default();
            for tool in startup_permission_policies(config) {
                let behavior = match tool.permission_policy {
                    Some(McpToolPermissionPolicy::AlwaysDeny) => PermissionToolCheckBehavior::Deny,
                    Some(McpToolPermissionPolicy::AlwaysAsk) => PermissionToolCheckBehavior::Ask,
                    _ => continue,
                };
                let name = format!(
                    "mcp__{}__{}",
                    config.normalized_name_for_mcp(),
                    lingxi_core::types::normalize_name_for_mcp(&tool.name)
                );
                rules
                    .entry(name)
                    .and_modify(|current| {
                        if behavior == PermissionToolCheckBehavior::Deny {
                            *current = behavior;
                        }
                    })
                    .or_insert(behavior);
            }
        }
    }
    gate.replace_mcp_server_policy_rules(snapshot_permission_rules(registry, &connections));
}

/// Authoritative host replacement updates SDK tool policy rather than
/// accumulating initialize declarations belonging to removed generations.
pub async fn replace_sdk_control_policies(
    registry: &crate::McpRegistry,
    configs: &[McpServerConfig],
    removed: &[String],
    gate: &dyn PermissionGate,
) {
    let _publication = registry.permission_policy_publish_lock.lock().await;
    let connections = registry.connections.read().await.clone();
    {
        let mut declared = registry
            .sdk_initialize_permission_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in removed {
            declared.remove(name);
        }
        for config in configs
            .iter()
            .filter(|config| matches!(config.spec, McpTransportSpec::SdkControl { .. }))
        {
            let mut rules = std::collections::BTreeMap::new();
            for tool in startup_permission_policies(config) {
                let behavior = match tool.permission_policy {
                    Some(McpToolPermissionPolicy::AlwaysDeny) => PermissionToolCheckBehavior::Deny,
                    Some(McpToolPermissionPolicy::AlwaysAsk) => PermissionToolCheckBehavior::Ask,
                    _ => continue,
                };
                rules.insert(
                    format!(
                        "mcp__{}__{}",
                        config.normalized_name_for_mcp(),
                        lingxi_core::types::normalize_name_for_mcp(&tool.name)
                    ),
                    behavior,
                );
            }
            declared.insert(config.name.clone(), rules);
        }
    }
    gate.replace_mcp_server_policy_rules(snapshot_permission_rules(registry, &connections));
}

fn snapshot_permission_rules(
    registry: &crate::McpRegistry,
    connections: &std::collections::HashMap<String, crate::McpConnectionState>,
) -> Vec<PermissionToolCheckRule> {
    let mut rules = catalog_permission_rules(connections);
    let declared = registry
        .sdk_initialize_permission_rules
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (server, policies) in declared.iter() {
        // Missing entries are declared but not yet dialed. Explicitly retired
        // or failed connections keep the existing catalog retraction behavior.
        if let Some(state) = connections.get(server) {
            if state.config().disabled
                || !matches!(state.config().spec, McpTransportSpec::SdkControl { .. })
                || !matches!(
                    state,
                    crate::McpConnectionState::Connected { .. }
                        | crate::McpConnectionState::Cached { .. }
                        | crate::McpConnectionState::Connecting { .. }
                        | crate::McpConnectionState::Reconnecting { .. }
                        | crate::McpConnectionState::HealthChecking { .. }
                )
            {
                continue;
            }
        }
        rules.extend(
            policies
                .iter()
                .map(|(name, behavior)| PermissionToolCheckRule {
                    tool_name: name.clone(),
                    rule_content: None,
                    behavior: *behavior,
                    source: PermissionToolCheckRuleSource::McpServerPolicy,
                }),
        );
    }
    let rank = |behavior| match behavior {
        PermissionToolCheckBehavior::Deny => 2,
        PermissionToolCheckBehavior::Ask => 1,
        PermissionToolCheckBehavior::Allow => 0,
    };
    rules.sort_by(|a, b| {
        a.tool_name
            .cmp(&b.tool_name)
            .then_with(|| rank(b.behavior).cmp(&rank(a.behavior)))
    });
    rules.dedup_by(|a, b| a.tool_name == b.tool_name);
    rules
}

/// Whether this is the branded counterpart of Native's `CLAUDE_CODE_REMOTE`.
#[must_use]
pub fn is_remote_session() -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var(branding::REMOTE_ENV).ok().as_deref())
}

/// Native exempts only remote server-policy asks in bypass mode. Explicit
/// server-policy denials and organization ceilings remain enforced.
#[must_use]
pub fn remote_server_policy_bypass_exempt() -> bool {
    is_remote_session() && telemetry::flag_bool("tengu_mcp_server_policy_bypass_exempt", true)
}

/// Remote startup policy declarations become rules only behind this flag.
#[must_use]
pub fn startup_permission_policies(config: &McpServerConfig) -> Vec<McpConfiguredToolPolicyDto> {
    startup_permission_policies_with(
        config,
        is_remote_session(),
        telemetry::flag_bool("tengu_mcp_startup_policy_seed", true),
    )
}

/// Native 2.1.318 `Nyt`/`i4i` (@257937207/@257937772), parameterized on
/// session gates. SDK policies can tighten through ask/deny but cannot grant
/// always-allow. Other startup policies require a remote dynamic HTTP/SSE
/// config; a user, agent, or claude.ai connector entry does not qualify.
#[must_use]
pub fn startup_permission_policies_with(
    config: &McpServerConfig,
    remote: bool,
    seed_enabled: bool,
) -> Vec<McpConfiguredToolPolicyDto> {
    let sdk = matches!(config.spec, McpTransportSpec::SdkControl { .. });
    if !sdk {
        let ordinary_http_or_sse = matches!(
            config.spec,
            McpTransportSpec::Http { .. } | McpTransportSpec::Sse { .. }
        ) && config
            .metadata
            .transport
            .as_deref()
            .is_none_or(|transport| matches!(transport, "http" | "sse"));
        if !remote || !seed_enabled || config.scope != ConfigScope::Dynamic || !ordinary_http_or_sse
        {
            return Vec::new();
        }
    }
    config
        .tools
        .iter()
        .filter(|tool| {
            tool.permission_policy
                .is_some_and(|policy| !sdk || policy != McpToolPermissionPolicy::AlwaysAllow)
        })
        .cloned()
        .collect()
}

/// Derive the complete shared-session rule bucket from the same catalog
/// generation used to publish tool handles. Retired/disconnected servers and
/// agent-scoped table entries do not retain permissions in the parent session.
#[must_use]
pub fn catalog_permission_rules(
    connections: &std::collections::HashMap<String, crate::McpConnectionState>,
) -> Vec<PermissionToolCheckRule> {
    let mut policies = std::collections::BTreeMap::<String, McpToolPermissionPolicy>::new();
    for (key, state) in connections {
        let config = match state {
            crate::McpConnectionState::Connected { config, .. }
            | crate::McpConnectionState::Cached { config, .. }
            | crate::McpConnectionState::Connecting { config, .. }
            | crate::McpConnectionState::Reconnecting { config, .. }
            | crate::McpConnectionState::HealthChecking { config, .. } => config,
            _ => continue,
        };
        if key != &config.name || config.disabled {
            continue;
        }
        for tool in startup_permission_policies(config) {
            let name = format!(
                "mcp__{}__{}",
                lingxi_core::types::normalize_name_for_mcp(&config.name),
                lingxi_core::types::normalize_name_for_mcp(&tool.name)
            );
            let policy = tool
                .permission_policy
                .expect("startup gate retains only declared policies");
            policies
                .entry(name)
                .and_modify(|current| {
                    *current = match (*current, policy) {
                        (McpToolPermissionPolicy::AlwaysDeny, _)
                        | (_, McpToolPermissionPolicy::AlwaysDeny) => {
                            McpToolPermissionPolicy::AlwaysDeny
                        }
                        (McpToolPermissionPolicy::AlwaysAsk, _)
                        | (_, McpToolPermissionPolicy::AlwaysAsk) => {
                            McpToolPermissionPolicy::AlwaysAsk
                        }
                        _ => McpToolPermissionPolicy::AlwaysAllow,
                    };
                })
                .or_insert(policy);
        }
    }
    policies
        .into_iter()
        .map(|(tool_name, policy)| PermissionToolCheckRule {
            tool_name,
            rule_content: None,
            behavior: match policy {
                McpToolPermissionPolicy::AlwaysAllow => PermissionToolCheckBehavior::Allow,
                McpToolPermissionPolicy::AlwaysAsk => PermissionToolCheckBehavior::Ask,
                McpToolPermissionPolicy::AlwaysDeny => PermissionToolCheckBehavior::Deny,
            },
            source: PermissionToolCheckRuleSource::McpServerPolicy,
        })
        .collect()
}

/// Capture and publish under one shared lock. Connection-state mutations may
/// proceed while the gate is updated, but the next publisher must take a fresh
/// snapshot afterwards, so an old snapshot cannot erase newer restrictions.
/// The connection-state read lock is released before entering the gate.
pub async fn catalog_snapshot_with_permission_policies(
    registry: &crate::McpRegistry,
    gate: Option<&dyn PermissionGate>,
) -> std::collections::HashMap<String, crate::McpConnectionState> {
    let _publication = registry.permission_policy_publish_lock.lock().await;
    let connections = registry.connections.read().await.clone();
    if let Some(gate) = gate {
        gate.replace_mcp_server_policy_rules(snapshot_permission_rules(registry, &connections));
    }
    connections
}

/// Synchronize rules after an SDK registration/control lifecycle mutation.
pub async fn synchronize_permission_policies(
    registry: &crate::McpRegistry,
    gate: &dyn PermissionGate,
) {
    catalog_snapshot_with_permission_policies(registry, Some(gate)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_policy_snapshot_keeps_other_servers_and_retracts_retired_entries() {
        let config = |name: &str| {
            crate::build_server_from_json_entry(
                name,
                &serde_json::json!({"type":"sdk","name":name,"tools":[
                    {"name":"write","permission_policy":"always_deny"}
                ]}),
                ConfigScope::Dynamic,
            )
            .unwrap()
        };
        let connected = |config| crate::McpConnectionState::Connected {
            config,
            connection_id: lingxi_core::types::McpConnectionId::new(),
            capabilities: lingxi_core::host::ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                logging: false,
                directory_read: false,
                experimental: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                    .unwrap(),
                extensions: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                    .unwrap(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: lingxi_core::host::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
            connected_at: std::time::SystemTime::now(),
        };
        let mut connections = std::collections::HashMap::from([
            ("one".into(), connected(config("one"))),
            ("two".into(), connected(config("two"))),
            ("agent-scoped".into(), connected(config("child"))),
        ]);
        let names = |rules: Vec<PermissionToolCheckRule>| {
            rules
                .into_iter()
                .map(|rule| rule.tool_name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(catalog_permission_rules(&connections)),
            vec!["mcp__one__write", "mcp__two__write"]
        );
        connections.insert(
            "one".into(),
            crate::McpConnectionState::Stopped {
                config: config("one"),
            },
        );
        assert_eq!(
            names(catalog_permission_rules(&connections)),
            vec!["mcp__two__write"]
        );
        connections.remove("two");
        assert!(catalog_permission_rules(&connections).is_empty());
    }

    #[test]
    fn full_review_transient_dial_states_keep_declared_restrictions() {
        let config = crate::build_server_from_json_entry(
            "server",
            &serde_json::json!({
                "type":"sdk", "name":"server", "tools":[
                    {"name":"write","permission_policy":"always_deny"},
                    {"name":"read","permission_policy":"always_ask"}
                ]
            }),
            ConfigScope::Dynamic,
        )
        .unwrap();
        let mut connections = std::collections::HashMap::from([(
            "server".into(),
            crate::McpConnectionState::Connecting {
                config: config.clone(),
                started_at: std::time::SystemTime::now(),
            },
        )]);
        let during_connect = catalog_permission_rules(&connections);
        assert_eq!(during_connect.len(), 2);
        connections.insert(
            "server".into(),
            crate::McpConnectionState::Reconnecting {
                config,
                retry_count: 1,
                next_retry_at: std::time::SystemTime::now(),
            },
        );
        assert_eq!(catalog_permission_rules(&connections), during_connect);
    }

    #[test]
    fn startup_policy_scope_and_gates_match_318_native_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/claude_2_1_318_policy_upstream_urls.json"
        ))
        .unwrap();
        for case in fixture["startupPolicies"]["cases"].as_array().unwrap() {
            let scope = match case["scope"].as_str().unwrap() {
                "dynamic" => ConfigScope::Dynamic,
                "agent" => ConfigScope::Agent,
                "claudeai" => ConfigScope::ClaudeAi,
                "user" => ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
                other => panic!("unknown fixture scope {other}"),
            };
            let config = crate::build_server_from_json_entry(
                "srv",
                &serde_json::json!({
                    "type": case["type"],
                    "url": "https://mcp.test/endpoint",
                    "command": "mcp-test",
                    "id": "connector",
                    "name": "sdk-server",
                    "tools": case["tools"],
                }),
                scope,
            )
            .unwrap();
            let policies = startup_permission_policies_with(
                &config,
                case["remote"].as_bool().unwrap(),
                case["seedEnabled"].as_bool().unwrap(),
            );
            let mut actual = serde_json::json!({"allow": [], "ask": [], "deny": []});
            for tool in policies {
                let key = match tool.permission_policy.unwrap() {
                    McpToolPermissionPolicy::AlwaysAllow => "allow",
                    McpToolPermissionPolicy::AlwaysAsk => "ask",
                    McpToolPermissionPolicy::AlwaysDeny => "deny",
                };
                actual[key]
                    .as_array_mut()
                    .unwrap()
                    .push(format!("mcp__srv__{}", tool.name).into());
            }
            assert_eq!(actual, case["expected"], "{case}");
        }
    }
}
