//! Differential fixtures replay unchanged shipped 2.1.322 policy functions.
use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, SettingsScope};
use mcp::sdk_verdict::{
    apply_sdk_auto_mode_verdict, SdkAutoModeVerdictAuthority, SdkAutoModeVerdictHostFacts,
    SdkVerdictCatalogTool,
};
use mcp::{apply_sdk_mcp_settings, build_server_from_json_entry, ConfigScope, McpServerProvenance};
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/mcp_sdk_policy_2_1_322.json")).unwrap()
}
fn source(value: &Value) -> Source {
    Source::parse(&serde_json::to_string(value).unwrap()).unwrap()
}
fn sdk_config() -> mcp::McpServerConfig {
    build_server_from_json_entry(
        "host-key",
        &json!({"type":"sdk","name":"host-key"}),
        ConfigScope::Dynamic,
    )
    .unwrap()
}

#[test]
fn native_322_sdk_settings_limits_and_exact_utf16() {
    for case in fixture()["settingsCases"].as_array().unwrap() {
        let input = Source::parse(case["inputSource"].as_str().unwrap()).unwrap();
        let mut config = sdk_config();
        apply_sdk_mcp_settings(&mut config, &input);
        let mut fields = Vec::new();
        if config.metadata.sdk_disable_auto_background {
            fields.push(("disableAutoBackground".into(), source(&json!(true))));
        }
        if let Some(id) = config.metadata.sdk_connector_id {
            fields.push(("connectorId".into(), id));
        }
        if let Some(tools) = config.metadata.sdk_auto_mode_verdict_tools {
            fields.push(("autoModeVerdictTools".into(), tools));
        }
        assert_eq!(
            Source::object(fields).unwrap().to_json_string().unwrap(),
            case["expectedSource"].as_str().unwrap(),
            "{case}"
        );
    }
    let config = build_server_from_json_entry(
        "host-key",
        &json!({
            "type":"sdk","name":"host-key","connectorId":"untrusted",
            "autoModeVerdictTools":["ping"],"disableAutoBackground":true,
        }),
        ConfigScope::Dynamic,
    )
    .unwrap();
    assert!(config.metadata.sdk_connector_id.is_none());
    assert!(config.metadata.sdk_auto_mode_verdict_tools.is_none());
    assert!(config.metadata.sdk_disable_auto_background);
}

#[test]
fn native_322_sdk_trust_provenance_and_connector_deny_keys() {
    for case in fixture()["trustCases"].as_array().unwrap() {
        let input = &case["config"];
        let scope = match input["scope"].as_str().unwrap() {
            "dynamic" => ConfigScope::Dynamic,
            "project" => ConfigScope::Settings(SettingsScope::Project),
            "user" => ConfigScope::Settings(SettingsScope::User),
            "agent" => ConfigScope::Agent,
            other => panic!("scope {other}"),
        };
        let entry = if input["type"] == "sdk" {
            json!({"type":"sdk","name":"host-key"})
        } else {
            json!({"type":"http","url":"http://127.0.0.1/mcp"})
        };
        let mut config = build_server_from_json_entry("host-key", &entry, scope).unwrap();
        config.metadata.relayed = input["relayed"] == true;
        if input.get("pluginSource").is_some() {
            config.metadata.provenance_source = Some("plugin".into());
        }
        apply_sdk_mcp_settings(&mut config, &source(input));
        assert_eq!(
            config.is_trusted_sdk(),
            case["trusted"].as_bool().unwrap(),
            "{case}"
        );
        assert_eq!(
            McpServerProvenance::from_config(&config).source,
            case["source"].as_str().unwrap(),
            "{case}"
        );
        assert_eq!(
            config.sdk_connector_deny_key().as_deref(),
            case["denyKey"].as_str(),
            "{case}"
        );
    }
}

fn authority(input: &Value) -> Option<SdkAutoModeVerdictAuthority> {
    let facts = SdkAutoModeVerdictHostFacts {
        enabled: input["enabled"] == true,
        auto_mode_classified: input["autoModeClassified"] == true,
        tool_use_id: input["toolUseId"].as_str().map(str::to_owned),
        hook_caller: input["hookCaller"] == true,
        client_requested_tool_use_id: input["clientRequestedToolUseId"]
            .as_str()
            .map(str::to_owned),
        parent_is_virtual: input["parentIsVirtual"] == true,
        parent_model_tool_uses: input["parentToolUses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row[0].as_str().unwrap().into(),
                    row[1].as_str().unwrap().into(),
                )
            })
            .collect(),
        main_conversation: input["mainConversation"] == true,
        pipeline_allows: input["pipelineAllows"] == true,
        pipeline_classifier: input["pipelineClassifier"] == true,
        pipeline_server_cleared: input["serverCleared"] == true,
        model_wire_input_text: input["wireInputText"].as_str().map(str::to_owned),
        query_tools: input["queryTools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| SdkVerdictCatalogTool {
                model_name: tool["modelName"].as_str().unwrap().into(),
                server_name: tool["serverName"].as_str().unwrap().into(),
                raw_tool_name: source(&tool["rawTool"]),
                source: tool["source"].as_str().unwrap().into(),
            })
            .collect(),
    };
    SdkAutoModeVerdictAuthority::from_host_facts(facts)
}

#[test]
fn native_322_sdk_verdict_requires_server_authority_and_hidden_raw_input() {
    for case in fixture()["verdictCases"].as_array().unwrap() {
        let input = &case["input"];
        let config_input = &input["config"];
        let entry = if config_input["type"] == "sdk" {
            json!({"type":"sdk","name":"host-key"})
        } else {
            json!({"type":"http","url":"http://127.0.0.1/mcp"})
        };
        let mut config =
            build_server_from_json_entry("host-key", &entry, ConfigScope::Dynamic).unwrap();
        apply_sdk_mcp_settings(&mut config, &source(config_input));
        let authority = authority(input);
        let mut meta = source(&json!({
            "claudecode/toolUseId":input["metaToolUseId"],
            "claudecode/autoModeVerdict":"forged", "other":"keep",
        }));
        apply_sdk_auto_mode_verdict(
            &mut meta,
            authority.as_ref(),
            &config,
            &source(&input["rawTool"]),
            &source(&input["args"]),
            input["toolUseId"].as_str(),
        );
        assert_eq!(
            meta.to_json_string().unwrap(),
            source(&case["expectedMeta"]).to_json_string().unwrap(),
            "{}",
            case["name"]
        );
    }
}
