use super::{mcp_servers_inventory_payload, mcp_tools_commands_loaded_payload};
use lingxi_core::host::McpTransportSpec;
use lingxi_core::types::McpConnectionId;
use std::collections::HashMap;

fn stdio_config(name: &str, scope: mcp::ConfigScope) -> mcp::McpServerConfig {
    mcp::McpServerConfig {
        name: name.to_string(),
        spec: McpTransportSpec::Stdio {
            command: "echo".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
        },
        scope,
        disabled: false,
        timeout_ms: None,
        discovery_cache: None,
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        config_error: None,
        metadata: Default::default(),
    }
}

#[test]
fn mcp_server_inventory_matches_oracle_scope_buckets_and_folds_managed_into_enterprise() {
    let payload = mcp_servers_inventory_payload(&[
        stdio_config("enterprise", mcp::ConfigScope::Enterprise),
        stdio_config(
            "managed",
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
        ),
        stdio_config(
            "global",
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        ),
        stdio_config(
            "project",
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        ),
        stdio_config(
            "user",
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Local),
        ),
        stdio_config("dynamic", mcp::ConfigScope::Dynamic),
        stdio_config("agent", mcp::ConfigScope::Agent),
        stdio_config("claudeai", mcp::ConfigScope::ClaudeAi),
    ]);

    assert_eq!(payload.enterprise, 2);
    assert_eq!(payload.global, 1);
    assert_eq!(payload.project, 1);
    assert_eq!(payload.user, 1);
    assert_eq!(payload.plugin, 1);
    assert_eq!(payload.agent, 1);
    assert_eq!(payload.claudeai, 1);
}

#[test]
fn mcp_tools_commands_loaded_uses_utf16_lengths_like_the_oracle_js_strings() {
    let prompts = [(
        "srv".to_string(),
        McpConnectionId::new(),
        lingxi_core::host::McpPromptDto {
            name: "emoji".to_string(),
            description: Some("desc😀".to_string()),
            arguments: vec![lingxi_core::host::McpPromptArgumentDto {
                name: "旗".to_string(),
                description: None,
                required: true,
            }],
        },
    )];
    let commands = command_api::mcp_prompts::mcp_prompt_commands(&prompts);
    let payload = mcp_tools_commands_loaded_payload(7, &commands);

    assert_eq!(payload.tools_count, 7);
    assert_eq!(payload.commands_count, 1);
    assert_eq!(
        payload.commands_metadata_length,
        "srv:emoji".encode_utf16().count() as u32
            + "desc😀".encode_utf16().count() as u32
            + "<旗>".encode_utf16().count() as u32
    );
}
