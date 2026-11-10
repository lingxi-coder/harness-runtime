//! Invocation-local SDK auto-mode verdict authority (native Ccr, 2.1.322).
//!
//! The ordinary local classifier cannot mint a server-cleared verdict. A host
//! that owns a real server-classifier response must supply these facts from
//! its permission pipeline and retained model wire-input marker.

use crate::{McpServerConfig, McpServerName};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// One actual model-visible tool record in the owning query's catalog.
#[derive(Debug, Clone)]
pub struct SdkVerdictCatalogTool {
    pub model_name: String,
    pub server_name: McpServerName,
    pub raw_tool_name: Utf16JsonProjection,
    pub source: String,
}

/// Facts supplied by the host that owns this exact model and permission turn.
/// This type has no model-input/JSON deserializer.
#[derive(Debug, Clone, Default)]
pub struct SdkAutoModeVerdictHostFacts {
    pub enabled: bool,
    pub auto_mode_classified: bool,
    pub tool_use_id: Option<String>,
    pub hook_caller: bool,
    pub client_requested_tool_use_id: Option<String>,
    pub parent_is_virtual: bool,
    pub parent_model_tool_uses: Vec<(String, String)>,
    pub main_conversation: bool,
    pub pipeline_allows: bool,
    pub pipeline_classifier: bool,
    pub pipeline_server_cleared: bool,
    /// The hidden text-as-written marker, when the raw model-wire producer
    /// actually retained one. Never synthesize it from parsed/revised args.
    pub model_wire_input_text: Option<String>,
    pub query_tools: Vec<SdkVerdictCatalogTool>,
}

/// Opaque authority bound to one model call, source catalog and wire input.
/// Children/direct callers cannot acquire it by cloning ordinary JSON metadata.
#[derive(Debug, Clone)]
pub struct SdkAutoModeVerdictAuthority {
    facts: SdkAutoModeVerdictHostFacts,
}

impl SdkAutoModeVerdictAuthority {
    /// Admit only a real main-conversation model call cleared by the server
    /// classifier. Local classifier approvals must keep `server_cleared` false.
    #[must_use]
    pub fn from_host_facts(facts: SdkAutoModeVerdictHostFacts) -> Option<Self> {
        let id = facts.tool_use_id.as_deref()?;
        (facts.enabled
            && facts.auto_mode_classified
            && !facts.hook_caller
            && facts.client_requested_tool_use_id.as_deref() != Some(id)
            && !facts.parent_is_virtual
            && facts.main_conversation
            && facts.pipeline_allows
            && facts.pipeline_classifier
            && facts.pipeline_server_cleared
            && facts.model_wire_input_text.is_some()
            && facts
                .parent_model_tool_uses
                .iter()
                .any(|(call_id, _)| call_id == id))
        .then_some(Self { facts })
    }

    /// Recheck the actual destination and executed arguments at each RPC,
    /// including after lazy dial/authentication retries. Native compares JSON
    /// text, so reordered keys and post-hook/approval rewrites cannot inherit it.
    #[must_use]
    pub fn authorizes(
        &self,
        config: &McpServerConfig,
        raw_tool_name: &Utf16JsonProjection,
        args: &Utf16JsonProjection,
        tool_use_id: Option<&str>,
    ) -> bool {
        let Some(raw_tool_units) = raw_tool_name.string_units("") else {
            return false;
        };
        if !config.is_trusted_sdk()
            || tool_use_id != self.facts.tool_use_id.as_deref()
            || !config
                .metadata
                .sdk_auto_mode_verdict_tools
                .as_ref()
                .is_some_and(|tools| {
                    (0..tools.array_len("").unwrap_or(0)).any(|index| {
                        tools.string_units(&format!("/{index}")) == Some(raw_tool_units.clone())
                    })
                })
        {
            return false;
        }
        let server_name = config.authored_name();
        let matching = self
            .facts
            .query_tools
            .iter()
            .filter(|tool| {
                tool.server_name == server_name
                    && tool.raw_tool_name.string_units("") == Some(raw_tool_units.clone())
            })
            .collect::<Vec<_>>();
        if matching.is_empty() || matching.iter().any(|tool| tool.source != "sdk") {
            return false;
        }
        if !self.facts.parent_model_tool_uses.iter().any(|(id, name)| {
            Some(id.as_str()) == tool_use_id && matching.iter().any(|tool| tool.model_name == *name)
        }) {
            return false;
        }
        args.to_json_string().ok().as_deref() == self.facts.model_wire_input_text.as_deref()
    }
}

/// Native Ccr's first operation is unconditional deletion of a forged verdict.
/// Only an admitted authority may add it back for this actual call.
pub fn apply_sdk_auto_mode_verdict(
    meta: &mut Utf16JsonProjection,
    authority: Option<&SdkAutoModeVerdictAuthority>,
    config: &McpServerConfig,
    raw_tool_name: &Utf16JsonProjection,
    args: &Utf16JsonProjection,
    tool_use_id: Option<&str>,
) {
    let _ = meta.remove_field("claudecode/autoModeVerdict");
    let own_meta_id = meta.string_at("/claudecode~1toolUseId");
    if own_meta_id == tool_use_id
        && authority
            .is_some_and(|authority| authority.authorizes(config, raw_tool_name, args, tool_use_id))
    {
        let _ = meta.set_field("claudecode/autoModeVerdict", "allowed_by_server".into());
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use bytes::Bytes;
    use lingxi_core::host::{McpNegotiatedProtocol, McpProtocolEra};
    use serde_json::json;
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tokio::sync::mpsc;

    fn authority() -> SdkAutoModeVerdictAuthority {
        SdkAutoModeVerdictAuthority::from_host_facts(SdkAutoModeVerdictHostFacts {
            enabled: true,
            auto_mode_classified: true,
            tool_use_id: Some("model-call".into()),
            parent_model_tool_uses: vec![("model-call".into(), "mcp__host-key__ping".into())],
            main_conversation: true,
            pipeline_allows: true,
            pipeline_classifier: true,
            pipeline_server_cleared: true,
            model_wire_input_text: Some("{}".into()),
            query_tools: vec![SdkVerdictCatalogTool {
                model_name: "mcp__host-key__ping".into(),
                server_name: "host-key".into(),
                raw_tool_name: "ping".into(),
                source: "sdk".into(),
            }],
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn native_322_sdk_verdict_reaches_legacy_and_modern_wire_only_with_exact_authority() {
        for modern in [false, true] {
            for case in ["allowed", "no-authority", "rewritten-input"] {
                let (peer_tx, incoming) = mpsc::channel::<Bytes>(8);
                let (outgoing, mut peer_rx) = mpsc::channel::<Bytes>(8);
                let connection = Arc::new(jsonrpc::Connection::new_streams(
                    incoming,
                    outgoing,
                    jsonrpc::Mode::Lines,
                ));
                let mut client = crate::client::McpClient::new_sdk_hosted(
                    "host-key",
                    PathBuf::from("/tmp"),
                    connection.clone(),
                )
                .await;
                if modern {
                    client = client.with_negotiated_protocol(McpNegotiatedProtocol {
                        era: McpProtocolEra::Modern,
                        version: "2026-07-28".into(),
                    });
                }
                let mut config = crate::build_server_from_json_entry(
                    "host-key",
                    &json!({"type":"sdk","name":"host-key"}),
                    crate::ConfigScope::Dynamic,
                )
                .unwrap();
                crate::apply_sdk_mcp_settings(
                    &mut config,
                    &Utf16JsonProjection::parse(r#"{"autoModeVerdictTools":["ping"]}"#).unwrap(),
                );
                let authority = (case != "no-authority").then(authority);
                let input = if case == "rewritten-input" {
                    r#"{"rewritten":true}"#
                } else {
                    "{}"
                };
                let handle = tokio::spawn(async move {
                    client
                        .call_tool_with_progress_and_sdk_verdict(
                            "mcp__host-key__ping",
                            Utf16JsonProjection::parse(input).unwrap(),
                            Some("model-call"),
                            None,
                            authority.as_ref().map(|authority| (authority, &config)),
                        )
                        .await
                });
                let frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], "ping");
                assert_eq!(
                    request["params"]["_meta"]["claudecode/toolUseId"],
                    "model-call"
                );
                assert_eq!(
                    request["params"]["_meta"]["claudecode/autoModeVerdict"].as_str(),
                    (case == "allowed").then_some("allowed_by_server"),
                    "{modern} {case}"
                );
                let mut result = json!({"content":[],"isError":false});
                if modern {
                    result["resultType"] = json!("complete");
                }
                let mut response = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                )
                .unwrap();
                response.push(b'\n');
                peer_tx.send(Bytes::from(response)).await.unwrap();
                tokio::time::timeout(Duration::from_secs(2), handle)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                connection.close();
            }
        }
    }
}
