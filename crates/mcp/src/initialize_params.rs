//! MCP `initialize` request parameters per spec §6.2 + claude-code TS
//! reference `services/mcp/client.ts` lines 985-1002.

use crate::identity::ClientInfo;
use serde::Serialize;
use serde_json::{Map, Value};

/// Latest MCP protocol version date the bundled client sends in `initialize`.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Current ordinary-transport wire shape contains roots.listChanged and
/// both form/URL elicitation modes. The client applies its frozen per-server
/// initialize mode before sending; an explicit Bare choice retains `{}`.
#[derive(Debug, Clone, Serialize)]
pub struct ClientCapabilities {
    /// `roots` capability marker — serialized as `{"listChanged": true}`.
    pub roots: Map<String, Value>,
    /// Form/URL capability modes, or the explicit empty Bare marker.
    pub elicitation: Map<String, Value>,
}

impl Default for ClientCapabilities {
    fn default() -> Self {
        let mut roots = Map::new();
        roots.insert("listChanged".to_string(), Value::Bool(true));
        Self {
            roots,
            elicitation: lingxi_core::host::McpElicitationMode::FormAndUrl
                .wire()
                .as_object()
                .expect("elicitation capability is an object")
                .clone(),
        }
    }
}

/// Body of the MCP `initialize` request.
///
/// Field names are serialized as camelCase (`protocolVersion`,
/// `clientInfo`) to match the claude-code reference wire format.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// MCP protocol version date — the value claude-code sends on initialize.
    /// claude-code creates its MCP `Client` with no `protocolVersion` override
    /// (`services/mcp/client.ts:985-1002`), so the SDK sends its
    /// `LATEST_PROTOCOL_VERSION`; at the pinned SDK (`@modelcontextprotocol/sdk`
    /// `^1.12.1` → 1.29.0, `types.js:2`) that is `2025-11-25`.
    pub protocol_version: &'static str,
    /// Capability advertisement; see [`ClientCapabilities`].
    pub capabilities: ClientCapabilities,
    /// Identity of the calling client; see [`ClientInfo`].
    pub client_info: ClientInfo,
}

impl Default for InitializeParams {
    fn default() -> Self {
        Self {
            protocol_version: LATEST_PROTOCOL_VERSION,
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_params_wire_shape_matches_claude_code() {
        let params = InitializeParams::default();
        let json = serde_json::to_value(&params).expect("serialize");

        // protocolVersion is the literal MCP date the SDK's
        // LATEST_PROTOCOL_VERSION resolves to (SDK 1.29.0 → 2025-11-25).
        assert_eq!(json["protocolVersion"], LATEST_PROTOCOL_VERSION);

        // capabilities is EXACTLY
        // {"roots": {"listChanged": true}, "elicitation": {"form":{},"url":{}}}.
        let caps = &json["capabilities"];
        assert!(caps.is_object(), "capabilities must be a JSON object");
        let caps_obj = caps.as_object().unwrap();
        assert_eq!(caps_obj.len(), 2, "capabilities must have exactly 2 keys");
        assert!(caps_obj.contains_key("roots"), "roots key required");
        assert!(
            caps_obj.contains_key("elicitation"),
            "elicitation key required"
        );
        assert!(caps["roots"].is_object(), "roots must be an object");
        // roots advertises listChanged:true (parity 2.1.207 J7n()).
        assert_eq!(
            caps["roots"],
            serde_json::json!({ "listChanged": true }),
            "roots must be {{\"listChanged\":true}}"
        );
        assert!(
            caps["elicitation"].is_object(),
            "elicitation must be an object"
        );
        assert_eq!(
            caps["elicitation"],
            serde_json::json!({"form": {}, "url": {}}),
            "current default advertises both form and URL elicitation",
        );

        // clientInfo is camelCase (NOT client_info).
        assert!(
            json.get("clientInfo").is_some(),
            "must be camelCase clientInfo"
        );
        assert!(json.get("client_info").is_none(), "no snake_case leak");
        assert_eq!(json["clientInfo"]["name"], "lingxi");
    }

    #[test]
    fn raw_wire_bytes_contain_literal_lingxi_marker() {
        // Lock the BYTES of the outgoing JSON-RPC payload.
        let params = InitializeParams::default();
        let bytes = serde_json::to_vec(&params).expect("serialize");
        let s = std::str::from_utf8(&bytes).expect("utf8");
        assert!(
            s.contains(r#""name":"lingxi""#),
            "wire bytes must contain literal \"name\":\"lingxi\", got: {s}",
        );
        // Current native capability bytes include form and URL modes.
        assert!(
            s.contains(r#""capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}"#),
            "wire bytes must contain the parity capability shape, got: {s}",
        );
    }
}
