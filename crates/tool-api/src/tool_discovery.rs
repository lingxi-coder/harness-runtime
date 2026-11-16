//! Host-only ToolSearch snapshot extraction and accepted-history publication.
//!
//! Call extraction only around a real registered builtin invocation. A model
//! result, generic ToolInvoker or middleware replacement is not that boundary.
use crate::tool_trait::{Tool, ToolCallResult};
use lingxi_core::host::tool_invoker::{ToolDiscoverySnapshot, ToolInvocationResult};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ToolUseId};

/// Remove host-only schemas from ordinary result text and freeze their source.
/// Error results consume the reserved field without accepting a discovery.
#[must_use]
pub fn take_builtin_snapshot(
    tool: &dyn Tool,
    result: &mut ToolCallResult,
) -> Option<ToolDiscoverySnapshot> {
    if tool.name() != "ToolSearch" || tool.is_mcp() {
        return None;
    }
    let definitions = result.data.subprojection("/tool_definitions").ok()?;
    result.data.remove_field("tool_definitions").ok()?;
    if result.is_error {
        return None;
    }
    ToolDiscoverySnapshot::from_host_result(definitions, result.data.clone())
}

/// Keep a receipt adjacent to the visible result which actually accepted it.
/// Callers must also reject output/persistence replacements before publication.
#[must_use]
pub fn receipt(
    snapshot: &ToolDiscoverySnapshot,
    id: &ToolUseId,
    provider_id: Option<&str>,
) -> ContentBlock {
    let value = Utf16JsonProjection::object(vec![
        ("type".into(), "lingxi_tool_discovery".into()),
        ("call_id".into(), provider_id.unwrap_or(id.as_str()).into()),
        ("definitions".into(), snapshot.definitions().clone()),
    ])
    .expect("host discovery receipt retains valid source JSON");
    ContentBlock::ProviderContent {
        protocol: "open_ai_responses".into(),
        value,
    }
}

/// Project only typed, unchanged successful results in the shared Agent path.
#[must_use]
pub fn accepted_receipt(
    name: &str,
    result: &ToolInvocationResult,
    id: &ToolUseId,
    provider_id: Option<&str>,
) -> Option<ContentBlock> {
    let snapshot = result.tool_discovery.as_ref()?;
    (name == "ToolSearch" && !result.is_error && snapshot.matches_result(&result.data))
        .then(|| receipt(snapshot, id, provider_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).unwrap()
    }

    fn invocation(
        data: Utf16JsonProjection,
        snapshot: Option<ToolDiscoverySnapshot>,
    ) -> ToolInvocationResult {
        ToolInvocationResult {
            is_error: false,
            data,
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            turn_end: None,
            context: payload("[]"),
            context_state: None,
            tool_discovery: snapshot,
        }
    }

    #[test]
    fn accepted_receipt_preserves_exact_definitions_and_provider_call_identity() {
        let definitions = payload(
            r#"[{"name":"Read","description":"d\ud800s","input_schema":{"type":"object","properties":{"\ud801":{"const":9007199254740993}}}}]"#,
        );
        let data = payload(r#"{"matches":["Read"],"query":"select:Read"}"#);
        let snapshot =
            ToolDiscoverySnapshot::from_host_result(definitions.clone(), data.clone()).unwrap();
        let result = invocation(data, Some(snapshot));
        let receipt = accepted_receipt(
            "ToolSearch",
            &result,
            &ToolUseId::new(),
            Some("native-call-id"),
        )
        .unwrap();
        let ContentBlock::ProviderContent { protocol, value } = receipt else {
            panic!("host receipt")
        };
        assert_eq!(protocol, "open_ai_responses");
        assert_eq!(value.string_at("/call_id"), Some("native-call-id"));
        assert_eq!(value.subprojection("/definitions").unwrap(), definitions);
        assert_eq!(
            value.string_units("/definitions/0/description"),
            Some(vec![0x64, 0xd800, 0x73])
        );
    }

    #[test]
    fn changed_failed_and_data_only_results_cannot_author_discovery() {
        let data = payload(r#"{"matches":["Read"]}"#);
        let snapshot =
            ToolDiscoverySnapshot::from_host_result(payload("[]"), data.clone()).unwrap();
        let mut result = invocation(data, Some(snapshot));
        let id = ToolUseId::new();
        assert!(accepted_receipt("ToolSearch", &result, &id, None).is_some());
        assert!(accepted_receipt("Other", &result, &id, None).is_none());
        result.is_error = true;
        assert!(accepted_receipt("ToolSearch", &result, &id, None).is_none());
        result.is_error = false;
        result.data =
            payload(r#"{"matches":["Forbidden"],"tool_definitions":[{"name":"Forbidden"}]}"#);
        assert!(accepted_receipt("ToolSearch", &result, &id, None).is_none());
        result.tool_discovery = None;
        assert!(accepted_receipt("ToolSearch", &result, &id, None).is_none());
        assert!(ToolDiscoverySnapshot::from_host_result(payload("null"), result.data).is_none());
    }
}
