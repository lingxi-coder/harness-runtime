//! Exact MCP name leaves in `--agents` JSON; scalar YAML remains unchanged.
use crate::definition::AgentMcpServerSpec;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::collections::HashMap;

pub(super) fn flag_source(
    raw: &str,
) -> Result<(serde_json::Value, HashMap<String, Vec<AgentMcpServerSpec>>), serde_json::Error> {
    match serde_json::from_str(raw) {
        Ok(value) => Ok((value, HashMap::new())),
        Err(original) => {
            let Ok(mut source) = Utf16JsonProjection::parse(raw) else {
                return Err(original);
            };
            let Some(entries) = source.object_entries() else {
                return Err(original);
            };
            let mut exact = HashMap::new();
            let mut removals = Vec::new();
            for entry in entries {
                let Ok(agent_name) = String::from_utf16(entry.code_units) else {
                    return Err(original);
                };
                let Some(servers) = entry.value.get_property("mcpServers") else {
                    continue;
                };
                let Ok(specs) = parse_specs(servers) else {
                    return Err(original);
                };
                if specs.iter().any(|spec| {
                    matches!(
                        spec,
                        AgentMcpServerSpec::BySourceName(_) | AgentMcpServerSpec::SourceRecord(_)
                    )
                }) {
                    exact.insert(agent_name.clone(), specs);
                    removals.push(agent_name);
                }
            }
            if exact.is_empty() {
                return Err(original);
            }
            for name in removals {
                let pointer = format!("/{}", name.replace('~', "~0").replace('/', "~1"));
                let Ok(mut definition) = source.subprojection(&pointer) else {
                    return Err(original);
                };
                if definition.remove_field("mcpServers").is_err()
                    || source.set_field(&name, definition).is_err()
                {
                    return Err(original);
                }
            }
            let Ok(json) = source.to_json_string() else {
                return Err(original);
            };
            serde_json::from_str(&json)
                .map(|value| (value, exact))
                .map_err(|_| original)
        }
    }
}

fn parse_specs(value: JsonValueView<'_>) -> Result<Vec<AgentMcpServerSpec>, ()> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let length = value.array_len().ok_or(())?;
    let mut specs = Vec::new();
    for index in 0..length {
        let item = value.get_index(index).ok_or(())?;
        if let Some(units) = item.code_units() {
            specs.push(match String::from_utf16(units) {
                Ok(name) => AgentMcpServerSpec::ByName(name),
                Err(_) => {
                    AgentMcpServerSpec::BySourceName(mcp::McpServerName::from_utf16(units.to_vec()))
                }
            });
            continue;
        }
        let entries = item.object_entries().ok_or(())?;
        for entry in &entries {
            let raw = entry
                .value
                .to_projection()
                .map_err(|_| ())?
                .to_json_string()
                .map_err(|_| ())?;
            let body = serde_json::from_str(&raw).map_err(|_| ())?;
            if !mcp::server_entry_shape_is_valid(&body) {
                return Err(());
            }
        }
        let projection = item.to_projection().map_err(|_| ())?;
        if entries
            .iter()
            .any(|entry| String::from_utf16(entry.code_units).is_err())
        {
            specs.push(AgentMcpServerSpec::SourceRecord(projection));
        } else {
            let raw = projection.to_json_string().map_err(|_| ())?;
            let serde_json::Value::Object(record) = serde_json::from_str(&raw).map_err(|_| ())?
            else {
                return Err(());
            };
            specs.push(AgentMcpServerSpec::Record(record));
        }
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_agent_mcp_names_survive_flag_parse_and_scoped_conversion() {
        let raw = r#"{"worker":{"description":"fixture","prompt":"fixture","mcpServers":[{"wire \ud800":{"command":"fake"}}]}}"#;
        let definitions = crate::catalog::parse_agents_from_flag_json_checked(raw).unwrap();
        let scoped = crate::mcp_servers::agent_mcp_specs_to_scoped_configs(
            &definitions[0],
            false,
            false,
            &[],
        );
        assert_eq!(scoped.len(), 1);
        assert_eq!(
            scoped[0].config.authored_name_units(),
            vec![119, 105, 114, 101, 32, 0xd800]
        );
        assert_eq!(scoped[0].config.normalized_name_for_mcp(), "wire__");
        assert!(
            serde_json::to_string(&mcp::McpServerProvenance::from_config(&scoped[0].config))
                .unwrap()
                .contains(r#""name":"wire \ud800""#)
        );
    }
}
