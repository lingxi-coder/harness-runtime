use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use mcp::ConfigScope;
use migrations::global_config::{
    get_project_config, project_path_for_config, read_map as read_global_map,
};
use plugin::plugin_source_sha256;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::mcp_bridge::{remove_server, upsert_server, McpPaths};
use crate::settings_bridge::{apply_patch, build_snapshot, SettingsContext};
use client::protocol::commands::WritableScopeDto;

#[derive(Debug, Serialize)]
struct ScopeSnapshot {
    scope: &'static str,
    path: String,
    revision_sha256: String,
    raw_json: String,
}

#[derive(Debug, Serialize)]
struct ApprovalSnapshot {
    enabled_servers: Vec<String>,
    disabled_servers: Vec<String>,
    enable_all_project_servers: bool,
    legacy_source_present: bool,
    revision_sha256: String,
}

#[derive(Debug, Serialize)]
struct McpSnapshot {
    scopes: Vec<ScopeSnapshot>,
    runtime_servers: Vec<Value>,
    approval: ApprovalSnapshot,
}

pub fn snapshot_json(
    context: &SettingsContext,
    paths: &McpPaths,
    runtime_servers: Vec<Value>,
) -> Result<String, String> {
    let project_raw = read_text(&paths.project_dir.join(".mcp.json"))?;
    let global_raw = read_text(&paths.global_config_path)?;
    let global_map =
        read_global_map(&paths.global_config_path).map_err(|error| error.to_string())?;
    let project_key = project_path_for_config(&paths.project_dir);
    let local_map = get_project_config(&paths.global_config_path, &project_key)
        .map_err(|error| error.to_string())?;
    let settings_snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let local_layer = Value::Object(
        settings_snapshot
            .layers
            .get("local")
            .cloned()
            .unwrap_or_default(),
    );
    let approval = approval_snapshot(&local_layer, &global_raw, &paths.project_dir);
    let snapshot = McpSnapshot {
        scopes: vec![
            ScopeSnapshot {
                scope: "user",
                path: paths.global_config_path.to_string_lossy().into_owned(),
                revision_sha256: plugin_source_sha256(global_raw.as_bytes()),
                raw_json: serde_json::to_string_pretty(&json!({
                    "mcpServers": global_map.get("mcpServers").cloned().unwrap_or_else(|| Value::Object(Map::new()))
                }))
                .map_err(|error| error.to_string())?,
            },
            ScopeSnapshot {
                scope: "local",
                path: paths.global_config_path.to_string_lossy().into_owned(),
                revision_sha256: plugin_source_sha256(
                    serde_json::to_string(&Value::Object(local_map.clone()))
                        .map_err(|error| error.to_string())?
                        .as_bytes(),
                ),
                raw_json: serde_json::to_string_pretty(&json!({
                    "mcpServers": local_map.get("mcpServers").cloned().unwrap_or_else(|| Value::Object(Map::new()))
                }))
                .map_err(|error| error.to_string())?,
            },
            ScopeSnapshot {
                scope: "project",
                path: paths
                    .project_dir
                    .join(".mcp.json")
                    .to_string_lossy()
                    .into_owned(),
                revision_sha256: plugin_source_sha256(project_raw.as_bytes()),
                raw_json: project_raw,
            },
        ],
        runtime_servers,
        approval,
    };
    serde_json::to_string(&snapshot).map_err(|error| error.to_string())
}

pub fn save_server_entry(
    paths: &McpPaths,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<(), String> {
    let payload = parse_payload(payload_json)?;
    let scope = required_string(&payload, "scope")?;
    let name = required_string(&payload, "name")?;
    let config = payload
        .get("config")
        .cloned()
        .ok_or_else(|| "payload is missing `config`".to_string())?;
    validate_single_server(scope, name, &config)?;
    ensure_scope_revision(paths, scope, revision_sha256)?;
    upsert_server(paths, scope_from_str(scope)?, name, config)
}

pub fn remove_server_entry(
    paths: &McpPaths,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<(), String> {
    let payload = parse_payload(payload_json)?;
    let scope = required_string(&payload, "scope")?;
    let name = required_string(&payload, "name")?;
    ensure_scope_revision(paths, scope, revision_sha256)?;
    remove_server(paths, scope_from_str(scope)?, name)
}

pub fn set_project_approval(
    context: &SettingsContext,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<(), String> {
    let payload = parse_payload(payload_json)?;
    let snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let local_layer = Value::Object(snapshot.layers.get("local").cloned().unwrap_or_default());
    let actual = plugin_source_sha256(local_layer.to_string().as_bytes());
    if actual != revision_sha256 {
        return Err(format!(
            "revision conflict: expected {revision_sha256}, found {actual}"
        ));
    }
    let name = required_string(&payload, "name")?;
    let decision = required_string(&payload, "decision")?;
    let mut enabled = string_list(local_layer.get("enabledMcpjsonServers"));
    let mut disabled = string_list(local_layer.get("disabledMcpjsonServers"));
    enabled.retain(|entry| entry != name);
    disabled.retain(|entry| entry != name);
    let enable_all = match decision {
        "approve_all" => true,
        "clear" => false,
        _ => local_layer
            .get("enableAllProjectMcpServers")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    match decision {
        "approve" => enabled.push(name.to_string()),
        "reject" => disabled.push(name.to_string()),
        "clear" | "approve_all" => {}
        _ => return Err(format!("unknown approval decision `{decision}`")),
    }
    apply_patch(
        &context.paths,
        WritableScopeDto::Local,
        vec![
            (
                "enabledMcpjsonServers".to_string(),
                Some(Value::Array(
                    enabled.into_iter().map(Value::String).collect(),
                )),
            ),
            (
                "disabledMcpjsonServers".to_string(),
                Some(Value::Array(
                    disabled.into_iter().map(Value::String).collect(),
                )),
            ),
            (
                "enableAllProjectMcpServers".to_string(),
                Some(Value::Bool(enable_all)),
            ),
        ],
    )
}

fn read_text(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("{}".to_string()),
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

fn parse_payload(payload_json: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(payload_json) {
        Ok(Value::Object(payload)) => Ok(payload),
        Ok(_) => Err("payload must be a JSON object".to_string()),
        Err(error) => Err(format!("payload is not valid JSON: {error}")),
    }
}

fn required_string<'a>(payload: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("payload is missing `{key}`"))
}

fn scope_from_str(scope: &str) -> Result<client::protocol::commands::WritableScopeDto, String> {
    match scope {
        "user" => Ok(client::protocol::commands::WritableScopeDto::User),
        "local" => Ok(client::protocol::commands::WritableScopeDto::Local),
        "project" => Ok(client::protocol::commands::WritableScopeDto::Project),
        _ => Err(format!("unknown mcp scope `{scope}`")),
    }
}

fn validate_single_server(scope: &str, name: &str, config: &Value) -> Result<(), String> {
    let raw = serde_json::to_string(&json!({ "mcpServers": { name: config } }))
        .map_err(|error| error.to_string())?;
    let parsed = mcp::parse_mcp_json_string(&raw, config_scope(scope)?)
        .map_err(|error| format!("invalid MCP server config for `{name}`: {error}"))?;
    if parsed.len() == 1 && parsed[0].name == name {
        Ok(())
    } else {
        Err(format!(
            "invalid MCP server config for `{name}`: the runtime parser rejected the entry"
        ))
    }
}

fn config_scope(scope: &str) -> Result<ConfigScope, String> {
    match scope {
        "user" => Ok(ConfigScope::Settings(
            lingxi_core::types::SettingsScope::User,
        )),
        "local" => Ok(ConfigScope::Settings(
            lingxi_core::types::SettingsScope::Local,
        )),
        "project" => Ok(ConfigScope::Settings(
            lingxi_core::types::SettingsScope::Project,
        )),
        _ => Err(format!("unknown mcp scope `{scope}`")),
    }
}

fn ensure_scope_revision(paths: &McpPaths, scope: &str, expected: &str) -> Result<(), String> {
    let raw = match scope {
        "user" => read_text(&paths.global_config_path)?,
        "local" => {
            let key = project_path_for_config(&paths.project_dir);
            let local = get_project_config(&paths.global_config_path, &key)
                .map_err(|error| error.to_string())?;
            serde_json::to_string(&Value::Object(local)).map_err(|error| error.to_string())?
        }
        "project" => read_text(&paths.project_dir.join(".mcp.json"))?,
        _ => return Err(format!("unknown mcp scope `{scope}`")),
    };
    let actual = plugin_source_sha256(raw.as_bytes());
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "revision conflict: expected {expected}, found {actual}"
        ))
    }
}

fn approval_snapshot(
    local_layer: &Value,
    global_raw: &str,
    project_dir: &Path,
) -> ApprovalSnapshot {
    let project_key = migrations::global_config::project_path_for_config(project_dir);
    let legacy = serde_json::from_str::<Value>(global_raw)
        .ok()
        .and_then(|value| value.get("projects").cloned())
        .and_then(|projects| projects.get(&project_key).cloned())
        .unwrap_or(Value::Null);
    let enabled = {
        let mut values = string_list(local_layer.get("enabledMcpjsonServers"));
        values.extend(string_list(legacy.get("enabledMcpjsonServers")));
        dedupe(values)
    };
    let disabled = {
        let mut values = string_list(local_layer.get("disabledMcpjsonServers"));
        values.extend(string_list(legacy.get("disabledMcpjsonServers")));
        dedupe(values)
    };
    ApprovalSnapshot {
        enabled_servers: enabled,
        disabled_servers: disabled,
        enable_all_project_servers: local_layer
            .get("enableAllProjectMcpServers")
            .and_then(Value::as_bool)
            .or_else(|| {
                legacy
                    .get("enableAllProjectMcpServers")
                    .and_then(Value::as_bool)
            })
            .unwrap_or(false),
        legacy_source_present: !legacy.is_null(),
        revision_sha256: plugin_source_sha256(local_layer.to_string().as_bytes()),
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeMap::new();
    for value in values {
        seen.insert(value.clone(), value);
    }
    seen.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_single_server_validation_accepts_every_editable_transport() {
        let cases = [
            json!({"command":"node","args":["server.js"],"env":{"MODE":"test"}}),
            json!({"type":"http","url":"https://example.test/mcp","headers":{"X-Test":"1"}}),
            json!({"type":"streamable-http","url":"https://example.test/mcp"}),
            json!({"type":"sse","url":"https://example.test/events"}),
            json!({"type":"ws","url":"wss://example.test/mcp"}),
        ];
        for config in cases {
            validate_single_server("user", "server", &config)
                .unwrap_or_else(|error| panic!("transport should validate: {error}"));
        }
    }

    #[test]
    fn strict_single_server_validation_rejects_entries_the_parser_skips() {
        for config in [
            json!({"type":"http","url":3}),
            json!({"type":"websocket","url":"wss://example.test/mcp"}),
            json!({"command":"node","timeout":0}),
        ] {
            let error = validate_single_server("project", "broken", &config)
                .expect_err("silently skipped entries must be rejected before write");
            assert!(error.contains("runtime parser rejected"), "{error}");
        }
    }
}
