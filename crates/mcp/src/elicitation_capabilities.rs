//! Current 2.1.287 capability selection; a bare config affects initialize only.

use crate::connection::McpServerConfig;
use lingxi_core::host::{McpElicitationCapabilities, McpElicitationMode, McpTransportSpec};

pub(crate) fn resolve(
    transport: Option<&str>,
    bare: Option<bool>,
    url_enabled: bool,
    legacy_url_enabled: bool,
    ccr_proxy: bool,
    denylisted: bool,
) -> McpElicitationCapabilities {
    let modern = if url_enabled {
        McpElicitationMode::FormAndUrl
    } else {
        McpElicitationMode::Bare
    };
    let eligible = matches!(transport, None | Some("stdio" | "http" | "sse" | "ws"));
    McpElicitationCapabilities {
        legacy: if legacy_url_enabled && eligible && !ccr_proxy && !denylisted && bare != Some(true)
        {
            modern
        } else {
            McpElicitationMode::Bare
        },
        modern,
    }
}

pub(crate) fn for_config(config: &McpServerConfig) -> McpElicitationCapabilities {
    for_config_with_flags(
        config,
        telemetry::flag_bool("tengu_mcp_url_elicitation", true),
        telemetry::flag_bool("tengu_mcp_legacy_url_elicitation", true),
        &telemetry::flag_string_list("tengu_mcp_legacy_url_elicitation_server_denylist", &[]),
    )
}

fn for_config_with_flags(
    config: &McpServerConfig,
    url_enabled: bool,
    legacy_url_enabled: bool,
    denylist: &[String],
) -> McpElicitationCapabilities {
    let transport = config
        .metadata
        .transport
        .as_deref()
        .unwrap_or(config.spec.kind());
    let transport = if transport == "streamable-http" {
        "http"
    } else {
        transport
    };
    let url = match &config.spec {
        McpTransportSpec::Http { url, .. }
        | McpTransportSpec::Sse { url, .. }
        | McpTransportSpec::WebSocket { url, .. }
        | McpTransportSpec::SseIde { url, .. }
        | McpTransportSpec::WsIde { url, .. } => Some(url.as_str()),
        _ => None,
    };
    let denylisted = denylist.iter().any(|value| value == "*")
        || url
            .and_then(|url| url::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(str::to_lowercase))
            .is_some_and(|host| {
                denylist
                    .iter()
                    .map(String::as_str)
                    .filter(|entry| !entry.is_empty())
                    .any(|entry| {
                        let entry = entry.to_lowercase();
                        host == entry || host.ends_with(&format!(".{entry}"))
                    })
            });
    resolve(
        Some(transport),
        config.metadata.bare_elicitation_capability,
        url_enabled,
        legacy_url_enabled,
        transport == "ccr-proxy",
        denylisted,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connection::ConfigScope, json_config::build_server_from_json_entry};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};

    #[test]
    fn capability_selection_matches_540_native_boolean_and_default_cases() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/elicitation_capabilities_2_1_287.json"
        ))
        .unwrap();
        assert_eq!(fixture["version"], "2.1.287");
        assert_eq!(
            fixture["sha256"],
            "6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea"
        );
        let source = &fixture["source_slices"][0];
        let source_text = source["source"].as_str().unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(source_text.as_bytes())),
            source["sha256"].as_str().unwrap()
        );
        assert_eq!(source["start"], 186214909);
        assert_eq!(source["end"], 186216207);
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 540);
        for case in cases {
            let input = &case["input"];
            let flags = &input["flags"];
            let actual = resolve(
                input["server"]["type"].as_str(),
                input["server"]["bareElicitationCapability"].as_bool(),
                flags["tengu_mcp_url_elicitation"].as_bool().unwrap_or(true),
                flags["tengu_mcp_legacy_url_elicitation"]
                    .as_bool()
                    .unwrap_or(true),
                input["ccr_proxy"].as_bool().unwrap(),
                input["denylisted"].as_bool().unwrap(),
            );
            assert_eq!(
                json!({"modern_lsn": actual.modern.wire(), "legacy_klt": actual.legacy.wire()}),
                case["expected"],
                "{}",
                case["name"]
            );
        }
    }

    fn config(entry: Value) -> McpServerConfig {
        build_server_from_json_entry("srv", &entry, ConfigScope::Dynamic).unwrap()
    }

    #[test]
    fn config_and_host_authority_keep_modern_full_across_initialize_exclusions() {
        let mut http = config(json!({"type": "http", "url": "https://Sub.Example.test/mcp"}));
        let full = McpElicitationMode::FormAndUrl;
        let bare = McpElicitationMode::Bare;
        assert_eq!(for_config_with_flags(&http, true, true, &[]).legacy, full);
        for value in ["example.test", "SUB.EXAMPLE.TEST", "*"] {
            let actual = for_config_with_flags(&http, true, true, &[value.into()]);
            assert_eq!(
                actual,
                McpElicitationCapabilities {
                    legacy: bare,
                    modern: full
                }
            );
        }
        for value in ["ample.test", "unrelated.test", ""] {
            assert_eq!(
                for_config_with_flags(&http, true, true, &[value.into()]).legacy,
                full
            );
        }
        http.metadata.bare_elicitation_capability = Some(false);
        assert_eq!(for_config_with_flags(&http, true, true, &[]).legacy, full);
        http.metadata.bare_elicitation_capability = Some(true);
        assert_eq!(
            for_config_with_flags(&http, true, true, &[]),
            McpElicitationCapabilities {
                legacy: bare,
                modern: full
            }
        );
        http.metadata.bare_elicitation_capability = None;
        for transport in ["sse-ide", "ws-ide", "sdk", "claudeai-proxy", "ccr-proxy"] {
            http.metadata.transport = Some(transport.into());
            assert_eq!(
                for_config_with_flags(&http, true, true, &[]),
                McpElicitationCapabilities {
                    legacy: bare,
                    modern: full
                },
                "{transport}"
            );
        }
        http.metadata.transport = Some("streamable-http".into());
        assert_eq!(for_config_with_flags(&http, true, true, &[]).legacy, full);
        let stdio = config(json!({"command": "test-mcp"}));
        assert_eq!(
            for_config_with_flags(&stdio, true, true, &["*".into()]).legacy,
            bare
        );
        assert_eq!(
            for_config_with_flags(&stdio, true, true, &["example.test".into()]).legacy,
            full
        );
    }
}
