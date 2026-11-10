//! Source-preserving SDK server control intake; transport admission remains
//! owned by the shared MCP registry and the embedding's startup authority.
use lingxi_core::host::McpTransportSpec;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use mcp::{ConfigScope, McpServerConfig};

pub(super) fn reserved_name(name: &[u16]) -> bool {
    [
        "__proto__",
        "__defineGetter__",
        "__defineSetter__",
        "__lookupGetter__",
        "__lookupSetter__",
    ]
    .iter()
    .any(|value| name.iter().copied().eq(value.encode_utf16()))
}
fn reserved_error(name: &[u16]) -> String {
    format!(
        "\"{}\" is a reserved MCP server name and was not loaded",
        String::from_utf16_lossy(name)
    )
}
pub(super) fn validate_initialize(frame: &Json) -> Result<(), String> {
    for key in ["sdkMcpServers", "webSearchIsolationExemptMcpServers"] {
        if let Some(value) = frame.get_property(key).filter(|value| !value.is_null()) {
            if value.array_len().is_none_or(|count| {
                !(0..count).all(|index| {
                    value
                        .get_index(index)
                        .is_some_and(|value| value.code_units().is_some())
                })
            }) {
                return Err("initialize: sdkMcpServers and webSearchIsolationExemptMcpServers must be arrays of strings".into());
            }
        }
    }
    for index in 0..frame.array_len("/sdkMcpServers").unwrap_or(0) {
        let name = frame
            .string_units(&format!("/sdkMcpServers/{index}"))
            .unwrap();
        if reserved_name(&name) {
            return Err(format!("initialize: {}", reserved_error(&name)));
        }
    }
    Ok(())
}

pub(super) struct ParsedServers {
    pub configs: Vec<McpServerConfig>,
    pub errors: Json,
    pub denied_tools: Option<Vec<String>>,
}

pub(super) fn parse_set_servers(request: &Json, relayed: bool) -> Result<ParsedServers, String> {
    let servers = request
        .subprojection("/servers")
        .map_err(|_| "mcp_set_servers: servers must be an object of config objects".to_owned())?;
    let entries = servers
        .object_entries()
        .filter(|entries| {
            entries
                .iter()
                .all(|entry| entry.value.object_entries().is_some())
        })
        .ok_or_else(|| "mcp_set_servers: servers must be an object of config objects".to_owned())?;
    let mut configs = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        let name = entry.code_units;
        if reserved_name(name) {
            errors.push((name.to_vec(), Json::from(reserved_error(name))));
            continue;
        }
        let source = entry
            .value
            .to_projection()
            .map_err(|error| error.to_string())?;
        let kind = source.string_at("/type");
        if matches!(kind, Some("sse-ide" | "ws-ide")) {
            errors.push((
                name.to_vec(),
                "Ignored: IDE transports are registered by the CLI itself, not mcp_set_servers"
                    .into(),
            ));
            continue;
        }
        let mut config = if kind == Some("sdk") {
            let name_source =
                Json::root_string(String::from_utf16_lossy(name), name.to_vec()).unwrap();
            let frame = Json::object(vec![(
                "request".into(),
                Json::object(vec![
                    (
                        "sdkMcpServers".into(),
                        Json::array(vec![name_source]).unwrap(),
                    ),
                    (
                        "sdkMcpServerConfigs".into(),
                        Json::object_units(vec![(name.to_vec(), source.clone())]).unwrap(),
                    ),
                ])
                .unwrap(),
            )])
            .unwrap();
            super::sdk_mcp_transport::sdk_servers_from_initialize(&frame)
                .pop()
                .expect("SDK control schema permits every string map key")
        } else {
            let raw = source
                .try_to_serde_value_with_array_budget(usize::MAX)
                .map_err(|error| error.to_string())?;
            match mcp::build_server_from_json_entry_utf16(name, &raw, ConfigScope::Dynamic) {
                Some(config) => config,
                None => {
                    let mut fallback = mcp::build_server_from_json_entry_utf16(
                        name,
                        &serde_json::json!({"command":"native-invalid-config"}),
                        ConfigScope::Dynamic,
                    )
                    .unwrap();
                    let error = if source.get_property("command").is_some_and(|value| {
                        !value.is_null()
                            && value.code_units().is_none()
                            && value.array_len().is_none()
                    }) {
                        "e.command?.includes is not a function. (In 'e.command?.includes(t)', 'e.command?.includes' is undefined)"
                    } else {
                        "Fe.includes is not a function. (In 'Fe.includes(\"timed out\")', 'Fe.includes' is undefined)"
                    };
                    fallback.config_error = Some(error.into());
                    fallback.metadata.transport = kind.map(str::to_owned);
                    fallback
                }
            }
        };
        config.metadata.relayed = relayed;
        if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
            mcp::apply_sdk_mcp_settings(&mut config, &source);
        }
        configs.push(config);
    }
    mcp::finalize_mcp_server_names(&mut configs);
    let denied_tools = request.get_property("denied_tools").and_then(|list| {
        let count = list.array_len().filter(|count| *count <= 2048)?;
        (0..count)
            .map(|index| {
                let name = list.get_index(index)?.as_str()?;
                let mut parts = name.splitn(3, "__");
                (name.len() <= 512
                    && name
                        .bytes()
                        .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-'))
                    && parts.next() == Some("mcp")
                    && parts.next().is_some_and(|part| !part.is_empty())
                    && parts.next().is_some_and(|part| !part.is_empty()))
                .then(|| name.to_owned())
            })
            .collect::<Option<Vec<_>>>()
    });
    Ok(ParsedServers {
        configs,
        errors: Json::object_units(errors).unwrap(),
        denied_tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_322_server_control_intake_keeps_sdk_source_and_denied_tools_boundaries() {
        let source = Json::parse(r#"{"servers":{"\ud800":{"type":"sdk","name":"ignored","timeout":1234,"connectorId":"connector","autoModeVerdictTools":["read"],"scope":"user","pluginSource":"forged"},"__proto__":{"type":"sdk"},"constructor":{"type":"sdk"},"ide":{"type":"ws-ide","url":"ws://localhost"}},"denied_tools":["mcp__server__read"]}"#).unwrap();
        let parsed = parse_set_servers(&source, true).unwrap();
        assert_eq!(parsed.configs.len(), 2);
        let raw = parsed
            .configs
            .iter()
            .find(|config| config.authored_name_units() == [0xd800])
            .unwrap();
        assert_eq!(raw.scope, ConfigScope::Dynamic);
        assert!(raw.metadata.relayed);
        assert!(!raw.is_trusted_sdk());
        assert_eq!(raw.timeout_ms, Some(1234));
        assert_eq!(
            raw.metadata
                .sdk_connector_id
                .as_ref()
                .unwrap()
                .string_at(""),
            Some("connector")
        );
        assert_eq!(
            raw.metadata
                .sdk_auto_mode_verdict_tools
                .as_ref()
                .unwrap()
                .string_at("/0"),
            Some("read")
        );
        assert_eq!(
            parsed.errors.string_at("/__proto__"),
            Some("\"__proto__\" is a reserved MCP server name and was not loaded")
        );
        assert_eq!(
            parsed.errors.string_at("/ide"),
            Some("Ignored: IDE transports are registered by the CLI itself, not mcp_set_servers")
        );
        assert_eq!(parsed.denied_tools, Some(vec!["mcp__server__read".into()]));
        for raw in [
            r#"{"servers":null}"#,
            r#"{"servers":[]}"#,
            r#"{"servers":{"bad":null}}"#,
        ] {
            assert!(
                matches!(parse_set_servers(&Json::parse(raw).unwrap(), false), Err(error) if error == "mcp_set_servers: servers must be an object of config objects")
            );
        }
        for value in [
            r#"["mcp__server__"]"#,
            r#"["mcp__server__read💡"]"#,
            r#"[7]"#,
            "null",
        ] {
            let request =
                Json::parse(&format!("{{\"servers\":{{}},\"denied_tools\":{value}}}")).unwrap();
            assert!(parse_set_servers(&request, false)
                .unwrap()
                .denied_tools
                .is_none());
        }
        assert_eq!(
            parse_set_servers(
                &Json::parse(r#"{"servers":{},"denied_tools":[]}"#).unwrap(),
                false
            )
            .unwrap()
            .denied_tools,
            Some(vec![])
        );
    }

    #[test]
    fn native_322_initialize_sdk_arrays_and_prototype_names_are_validated_before_admission() {
        for source in [
            r#"{"sdkMcpServers":7}"#,
            r#"{"webSearchIsolationExemptMcpServers":[null]}"#,
        ] {
            assert_eq!(validate_initialize(&Json::parse(source).unwrap()).unwrap_err(), "initialize: sdkMcpServers and webSearchIsolationExemptMcpServers must be arrays of strings");
        }
        assert_eq!(
            validate_initialize(&Json::parse(r#"{"sdkMcpServers":["__proto__"]}"#).unwrap())
                .unwrap_err(),
            "initialize: \"__proto__\" is a reserved MCP server name and was not loaded"
        );
        validate_initialize(&Json::parse(r#"{"sdkMcpServers":["constructor","prototype","\ud800"],"webSearchIsolationExemptMcpServers":null}"#).unwrap()).unwrap();
    }
}
