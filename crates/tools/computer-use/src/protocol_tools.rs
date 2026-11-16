//! In-process MCP facade over the single trusted Computer Use session owner.
//!
//! Each advertised descriptor is retained from the hash-pinned 2.1.325 oracle.
//! Registering a wrapper never creates a second allowlist, lock or frame cache.
use super::ComputerTool;
use async_trait::async_trait;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, McpConnectionId};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tool_api::{
    context::ToolUseContext,
    progress::ToolProgressSender,
    tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
        ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance,
        ToolStaticContext,
    },
    ToolRegistry,
};

const SERVER: &str = "computer-use";
const NATIVE_SHA256: &str = "84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2";
static CATALOG: Lazy<Value> = Lazy::new(|| {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/protocol-2.1.325.json"))
        .expect("reviewed protocol fixture is valid JSON");
    assert_eq!(fixture["binarySha256"], NATIVE_SHA256);
    fixture
});
static MCP_OBJECT_INPUT: Lazy<Utf16JsonProjection> =
    Lazy::new(|| Utf16JsonProjection::parse(r#"{"type":"object"}"#).expect("literal schema"));
static OWNER_RESTRICTIVE_ALIASES: Lazy<Vec<String>> = Lazy::new(|| vec!["computer".into()]);

#[derive(Clone)]
struct Descriptor {
    raw: String,
    action: String,
    schema: Utf16JsonProjection,
    description: Utf16JsonProjection,
    fingerprint: String,
}

fn fingerprint(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

/// Select only a capability profile actually established by the host. An
/// unsupported profile yields no tools rather than borrowing stronger claims.
fn descriptors(capabilities: &Value) -> Vec<Descriptor> {
    let Some(case) = CATALOG["cases"].as_array().and_then(|cases| {
        cases.iter().find(|case| {
            &case["capabilities"] == capabilities
                && case["coordinateMode"] == "pixels"
                && case["installedApps"].as_array().is_some_and(Vec::is_empty)
        })
    }) else {
        return Vec::new();
    };
    case["descriptorSha256"]
        .as_array()
        .expect("fixture descriptor references")
        .iter()
        .map(|reference| {
            let expected = reference.as_str().expect("descriptor digest");
            let raw = CATALOG["descriptorCorpus"][expected]
                .as_str()
                .expect("fixture descriptor source");
            assert_eq!(fingerprint(raw.as_bytes()), expected);
            let source = Utf16JsonProjection::parse(raw).expect("native descriptor JSON");
            Descriptor {
                raw: raw.to_owned(),
                action: source
                    .string_at("/name")
                    .expect("native tool name")
                    .to_owned(),
                schema: source
                    .subprojection("/inputSchema")
                    .expect("native input schema"),
                description: source
                    .subprojection("/description")
                    .expect("native description"),
                fingerprint: expected.to_owned(),
            }
        })
        .collect()
}

pub(super) fn tool_names(capabilities: &Value) -> Vec<String> {
    descriptors(capabilities)
        .into_iter()
        .map(|descriptor| format!("mcp__{SERVER}__{}", descriptor.action))
        .collect()
}

/// Register the host's real in-process server partition. The slot/revision
/// identify this host configuration; no user-supplied FQN can mint its identity.
/// The internal `computer` owner must also stay registered for native receipts.
pub(super) fn register(registry: &mut ToolRegistry, owner: Arc<ComputerTool>) {
    let capabilities = owner.protocol_capabilities();
    let revision = fingerprint(
        serde_json::to_string(&json!({
            "transport":"in-process", "server":SERVER,
            "reference":NATIVE_SHA256, "capabilities":capabilities,
        }))
        .expect("trusted host configuration serializes")
        .as_bytes(),
    );
    let tools = descriptors(&capabilities)
        .into_iter()
        .map(|descriptor| {
            let identity = tool_api::McpToolIdentity {
                server_config_id: fingerprint(b"builtin:in-process:computer-use"),
                config_revision: revision.clone(),
                raw_tool_name: descriptor.action.clone(),
                definition_fingerprint: descriptor.fingerprint.clone(),
            };
            Arc::new(ProtocolTool {
                name: format!("mcp__{}__{}", SERVER, descriptor.action),
                descriptor,
                owner: owner.clone(),
                capabilities: capabilities.clone(),
                identity,
                provenance: mcp::McpServerProvenance {
                    name: SERVER.into(),
                    source: "builtin".into(),
                },
            }) as Arc<dyn Tool>
        })
        .collect::<Vec<_>>();
    registry.register_mcp_tools(McpConnectionId::new(), tools);
}

struct ProtocolTool {
    name: String,
    descriptor: Descriptor,
    owner: Arc<ComputerTool>,
    capabilities: Value,
    identity: tool_api::McpToolIdentity,
    provenance: mcp::McpServerProvenance,
}

#[async_trait]
impl Tool for ProtocolTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_mcp(&self) -> bool {
        true
    }
    fn mcp_tool_identity(&self) -> Option<&tool_api::McpToolIdentity> {
        Some(&self.identity)
    }
    fn mcp_server_provenance(&self) -> Option<&mcp::McpServerProvenance> {
        Some(&self.provenance)
    }
    fn mcp_deny_aliases(&self) -> &[String] {
        &OWNER_RESTRICTIVE_ALIASES
    }
    fn computer_control_owner(&self) -> Option<Arc<dyn Tool>> {
        Some(self.owner.clone())
    }
    fn family_parent_tool_name(&self) -> Option<&str> {
        Some("computer")
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.descriptor.schema
    }
    fn input_validation_schema(&self) -> &Utf16JsonProjection {
        &MCP_OBJECT_INPUT
    }
    fn input_schema_revision(&self) -> Option<String> {
        Some(self.descriptor.fingerprint.clone())
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::SourceNative(self.descriptor.schema.clone()),
            schema_provenance: ToolInputSchemaProvenance::SourceNative,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.owner.ctx.computer_control.is_some()
            && self
                .owner
                .policy_snapshot()
                .is_ok_and(|policy| policy.is_none_or(|(_, policy)| policy.enabled))
            && self.owner.protocol_capabilities() == self.capabilities
    }
    fn max_result_size_chars(&self) -> usize {
        30_000
    }
    fn persistence_threshold(&self) -> Option<usize> {
        Some(100_000)
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        matches!(
            self.descriptor.action.as_str(),
            "screenshot"
                | "zoom"
                | "cursor_position"
                | "list_granted_applications"
                | "list_apps"
                | "read_clipboard"
                | "wait"
                | "app_list_windows"
                | "app_screenshot"
                | "app_ax_find"
        )
    }
    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        self.owner.check_permissions(input, ctx).await
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        self.descriptor
            .description
            .string_at("")
            .expect("native description string")
            .to_owned()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        self.descriptor.description.clone()
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.owner
            .call_protocol(&self.descriptor.action, input, ctx, tx)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_reviewed_descriptor_profile_preserves_order_and_exact_native_bytes() {
        for case in CATALOG["cases"].as_array().unwrap() {
            if case["coordinateMode"] != "pixels"
                || !case["installedApps"].as_array().unwrap().is_empty()
            {
                continue;
            }
            let selected = descriptors(&case["capabilities"]);
            let names = selected
                .iter()
                .map(|d| d.action.as_str())
                .collect::<Vec<_>>();
            let expected = case["toolNames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(names, expected, "{}", case["name"]);
            let bytes = format!(
                "[{}]",
                selected
                    .iter()
                    .map(|d| d.raw.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            assert_eq!(
                fingerprint(bytes.as_bytes()),
                case["toolsJsonSha256"].as_str().unwrap()
            );
        }
    }
    /// Exercises production registration, visibility and wire projection, then
    /// the real SDK encoder. SDK construction below bridges the host wire type;
    /// this test does not invoke llm-runtime's separate lowering function.
    #[tokio::test]
    async fn registered_tools_preserve_native_fields_through_sdk_encoding() {
        use crate::execution_tests::{make_tool, RecordingBackend};
        use lingxi_llm_client::protocol::{ChatRequest, JsonPayload, ProviderProfile, ToolSpec};
        use lingxi_llm_client::{
            AnthropicMessagesCodec, CodecContext, EncodeRequest, RequestMode, WireCodec,
        };
        let owner = Arc::new(make_tool(Arc::new(RecordingBackend::default())));
        let capabilities = owner.protocol_capabilities();
        assert_eq!(
            crate::protocol_tool_names_for_host(
                owner.ctx.computer_control.as_deref().unwrap(),
                owner.access_resolver.supports_interactions()
            ),
            tool_names(&capabilities)
        );
        assert_eq!(capabilities["screenshotFiltering"], "none");
        let mut expected = descriptors(&capabilities);
        assert!(!expected.is_empty());
        expected.sort_by(|a, b| {
            tool_api::wire::locale_cmp(
                &format!("mcp__computer-use__{}", a.action),
                &format!("mcp__computer-use__{}", b.action),
            )
        });
        let mut registry = ToolRegistry::new();
        register(&mut registry, owner);
        let available = registry.available_tools(&ToolStaticContext::default());
        assert!(available
            .iter()
            .all(|tool| tool.mcp_deny_aliases() == ["computer"]));
        let definitions =
            tool_api::wire::tools_to_wire(&available, &PromptOptions::default()).await;
        assert_eq!(definitions.len(), expected.len());
        let mut request = ChatRequest::new("claude-test");
        request.max_tokens = Some(4096);
        for (definition, descriptor) in definitions.iter().zip(&expected) {
            assert_eq!(
                definition.name,
                format!("mcp__computer-use__{}", descriptor.action)
            );
            assert_eq!(
                definition.description.to_json_string().unwrap(),
                descriptor.description.to_json_string().unwrap()
            );
            assert_eq!(
                definition.input_schema.to_json_string().unwrap(),
                descriptor.schema.to_json_string().unwrap()
            );
            request.tools.push(ToolSpec {
                name: definition.name.clone(),
                description: JsonPayload::parse(&definition.description.to_json_string().unwrap())
                    .unwrap(),
                input_schema: JsonPayload::parse(
                    &definition.input_schema.to_json_string().unwrap(),
                )
                .unwrap(),
                strict: definition.strict,
                defer_loading: definition.defer_loading,
                native_options: Vec::new(),
                tool_type: None,
                extra: JsonPayload::default(),
            });
        }
        let profile: ProviderProfile = serde_json::from_value(json!({
            "provider_id":"anthropic", "profile_name":"computer-protocol-test", "base_url":"https://api.anthropic.com",
            "protocol":lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages, "auth":"none",
            "models":[{"display_model":"claude-test","request_model":"claude-test","billing_model":"claude-test"}], "extra":{}
        })).unwrap();
        let context = CodecContext::new(&profile, "claude-test", RequestMode::Complete);
        let encoded = AnthropicMessagesCodec
            .encode_request(EncodeRequest::new(&request), &context)
            .unwrap();
        let body = JsonPayload::parse(std::str::from_utf8(&encoded.body).unwrap()).unwrap();
        for (index, descriptor) in expected.iter().enumerate() {
            assert_eq!(
                body.raw_source_at(&format!("/tools/{index}/name")).unwrap(),
                serde_json::to_string(&format!("mcp__computer-use__{}", descriptor.action))
                    .unwrap()
            );
            assert_eq!(
                body.raw_source_at(&format!("/tools/{index}/description"))
                    .unwrap(),
                descriptor.description.to_json_string().unwrap()
            );
            assert_eq!(
                body.raw_source_at(&format!("/tools/{index}/input_schema"))
                    .unwrap(),
                descriptor.schema.to_json_string().unwrap()
            );
        }
    }

    #[test]
    fn unknown_capability_profiles_never_advertise_stronger_tools() {
        assert!(descriptors(&json!({"platform":"other"})).is_empty());
        assert_eq!(lingxi_core::types::normalize_name_for_mcp(SERVER), SERVER);
    }
}
