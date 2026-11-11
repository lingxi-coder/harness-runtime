use super::image_tool_result_blocks;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::json;

fn source(value: &serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("test fixture is valid JSON")
}

fn serialized_blocks(blocks: Vec<Utf16JsonProjection>) -> String {
    Utf16JsonProjection::array(blocks)
        .expect("tool result blocks form an array")
        .to_json_string()
        .expect("tool result blocks serialize")
}

#[test]
fn image_result_becomes_the_binary_mapper_block_array() {
    let data = json!({
        "type": "image",
        "file": { "base64": "QUJD", "type": "image/png", "originalSize": 3 }
    });
    let blocks = image_tool_result_blocks(&source(&data)).expect("image data maps");
    // Byte shape of the binary mapper's `case "image"` — one image block,
    // source keys in written order (type, data, media_type).
    assert_eq!(
        serialized_blocks(blocks),
        r#"[{"type":"image","source":{"type":"base64","data":"QUJD","media_type":"image/png"}}]"#
    );
}

#[test]
fn bash_isimage_result_maps_stdout_data_uri_with_sniffed_media_type() {
    use super::bash_image_tool_result_blocks;
    // `iVBORw0KGgo=` = base64 of the 8-byte PNG magic. The URI CLAIMS jpeg —
    // the mapper must emit the SNIFFED type (image/png), per `hKn`/`Wfe`.
    let data = json!({
        "stdout": "data:image/jpeg;base64,iVBORw0KGgo=",
        "stderr": "",
        "interrupted": false,
        "isImage": true,
    });
    let blocks = bash_image_tool_result_blocks(&source(&data)).expect("sniffable data-URI maps");
    assert_eq!(
        serialized_blocks(blocks),
        r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]"#
    );
}

#[test]
fn bash_isimage_misses_fall_back_to_none() {
    use super::bash_image_tool_result_blocks;
    // Not flagged as image.
    assert!(bash_image_tool_result_blocks(&source(
        &json!({"stdout":"data:image/png;base64,iVBORw0KGgo=","isImage":false})
    ))
    .is_none());
    // Flagged, but stdout is not a data-URI → text fallback (`hKn` null).
    assert!(
        bash_image_tool_result_blocks(&source(&json!({"stdout":"plain text","isImage":true})))
            .is_none()
    );
    // Valid URI shape but undecodable base64.
    assert!(bash_image_tool_result_blocks(&source(
        &json!({"stdout":"data:image/png;base64,@@not-base64@@","isImage":true})
    ))
    .is_none());
    // Decodable but unrecognized magic (claimed image, actually text bytes).
    assert!(bash_image_tool_result_blocks(&source(
        &json!({"stdout":"data:image/png;base64,aGVsbG8gd29ybGQh","isImage":true})
    ))
    .is_none());
}

#[test]
fn non_image_and_malformed_results_map_to_none() {
    // Strict no-op for every other tool result shape.
    assert!(image_tool_result_blocks(&source(&json!({"type":"text","file":{}}))).is_none());
    assert!(image_tool_result_blocks(&source(&json!({"filePath":"/a","content":"x"}))).is_none());
    assert!(image_tool_result_blocks(&source(&json!("just a string"))).is_none());
    // `type:"image"` but missing/malformed file fields → None (text fallback).
    assert!(image_tool_result_blocks(&source(&json!({"type":"image"}))).is_none());
    assert!(image_tool_result_blocks(&source(&json!({"type":"image","file":{}}))).is_none());
    assert!(
        image_tool_result_blocks(&source(&json!({"type":"image","file":{"base64":"QQ=="}})))
            .is_none()
    );
}

fn encoded_history(messages: Vec<lingxi_core::types::ConversationMessage>) -> serde_json::Value {
    use llm_runtime::services::sdk::{self, WireCodec};
    let messages = llm_runtime::convert::to_llm_messages(
        llm_runtime::convert::normalize_messages_for_api(messages),
    )
    .unwrap();
    let (input, _) = llm_runtime::convert::history_input(
        "claude-opus-4-7",
        &messages,
        &[],
        &[],
        sdk::protocol::ProtocolFamily::AnthropicMessages,
    )
    .unwrap();
    let profile = serde_json::from_value(json!({
        "provider_id":"anthropic", "profile_name":"test", "base_url":"https://api.anthropic.com",
        "protocol":"anthropic_messages", "auth":"none", "models":[]
    }))
    .unwrap();
    let encoded = sdk::AnthropicMessagesCodec
        .encode_request(
            sdk::EncodeRequest::new(&input),
            &sdk::CodecContext::new(&profile, &input.model, sdk::RequestMode::Complete),
        )
        .unwrap();
    serde_json::from_slice(&encoded.body).unwrap()
}

fn count_wire_media(value: &serde_json::Value, kind: &str) -> usize {
    match value {
        serde_json::Value::Object(fields) => {
            usize::from(fields.get("type").and_then(|value| value.as_str()) == Some(kind))
                + fields
                    .values()
                    .map(|value| count_wire_media(value, kind))
                    .sum::<usize>()
        }
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| count_wire_media(value, kind))
            .sum(),
        _ => 0,
    }
}

struct ReadMediaFixture {
    data: Utf16JsonProjection,
    new_messages: Vec<lingxi_core::types::ConversationMessage>,
}

#[async_trait::async_trait]
impl tool_api::tool_trait::Tool for ReadMediaFixture {
    fn name(&self) -> &str {
        "Read"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<Utf16JsonProjection> =
            once_cell::sync::Lazy::new(|| source(&json!({"type":"object", "properties":{}})));
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::context::ToolUseContext,
    ) -> Result<(), tool_api::tool_trait::ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        "read media fixture".into()
    }
    async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> Utf16JsonProjection {
        "".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::tool_trait::ToolCallResult, tool_api::tool_trait::ToolError> {
        Ok(tool_api::tool_trait::ToolCallResult {
            data: self.data.clone(),
            model_content: Some("Read summary".into()),
            new_messages: self.new_messages.clone(),
            context_modifier: None,
            mcp_meta: None,
            is_error: false,
            subagent_hooks_fired: false,
        })
    }
}

#[tokio::test]
async fn ordinary_read_dispatch_preserves_notebook_images_and_single_pdf_delivery_on_wire() {
    use crate::test_support::{MockApiClient, MockOutputStream, NoOpPermissionGate};
    use lingxi_core::types::{
        ContentBlock, ConversationMessage, DocumentSource, ImageSource, MessageId, ToolUseId,
    };
    use std::sync::Arc;
    let notebook = Utf16JsonProjection::parse(r#"{"type":"notebook","file":{"filePath":"plot.ipynb","cells":[{"cellType":"code","cell_id":"plot","language":"python","source":"display(plot)🙂","outputs":[{"text":"plot output"},{"image":{"image_data":"iVBORw0KGgo=","media_type":"image/png"}}]}]}}"#).unwrap();
    let pdf = source(&json!({"type":"pdf", "file":{
        "filePath":"report.pdf", "base64":"JVBERi0xLjQK", "originalSize":9
    }}));
    let pages = source(&json!({"type":"parts", "file":{
        "filePath":"report.pdf", "originalSize":9, "count":1
    }}));
    let cases = [
        (notebook, vec![], 1, 0),
        (
            pdf,
            vec![ConversationMessage::user_with_documents(
                MessageId::new(),
                String::new(),
                vec![DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: "JVBERi0xLjQK".into(),
                }],
            )],
            0,
            1,
        ),
        (
            pages,
            vec![ConversationMessage::user_with_images(
                MessageId::new(),
                String::new(),
                vec![ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "iVBORw0KGgo=".into(),
                }],
            )],
            1,
            0,
        ),
    ];
    for (data, new_messages, expected_images, expected_documents) in cases {
        let is_notebook = data.string_at("/type") == Some("notebook");
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(ReadMediaFixture { data, new_messages }));
        let orch = super::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            crate::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            "/tmp/wsp".into(),
        );
        let call = (ToolUseId::new(), "Read".into(), source(&json!({})), None);
        let assistant = ConversationMessage::Assistant {
            id: MessageId::new(),
            stop_reason: Some("tool_use".into()),
            content: vec![ContentBlock::ToolUse {
                id: call.0.clone(),
                name: "Read".into(),
                input: call.2.clone(),
                provider_id: None,
            }],
            per_turn_effort: None,
            resumable: None,
        };
        let (results, _, injected, _) = super::dispatch_tool_uses_tracked(&orch, &[call], None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1, "Read dispatch results={results:?}");
        // Ordinary successful builtins omit is_error unless their result
        // mapper explicitly supplies it. This fixture uses the default mapper.
        assert!(
            matches!(
                results.first(),
                Some(ContentBlock::ToolResult { is_error: None, .. })
            ),
            "successful Read fixture should omit is_error; results={results:?}"
        );
        if expected_documents == 1 {
            assert!(
                matches!(
                    results.first(),
                    Some(ContentBlock::ToolResult {
                        content_blocks: None,
                        ..
                    })
                ),
                "the injected PDF must remain the sole document owner; results={results:?}"
            );
            let injected_documents = injected
                .iter()
                .map(|(message, _)| match message {
                    ConversationMessage::User { content, .. } => content
                        .iter()
                        .filter(|block| matches!(block, ContentBlock::Document { .. }))
                        .count(),
                    _ => 0,
                })
                .sum::<usize>();
            assert_eq!(injected_documents, 1, "Read PDF rows={injected:?}");
        }
        let mut user = ConversationMessage::user(MessageId::new(), String::new());
        if let ConversationMessage::User { content, .. } = &mut user {
            *content = results;
        }
        let mut history = vec![assistant, user];
        history.extend(injected.into_iter().map(|(message, _)| message));
        let wire = encoded_history(history);
        assert_eq!(count_wire_media(&wire, "image"), expected_images, "{wire}");
        assert_eq!(
            count_wire_media(&wire, "document"),
            expected_documents,
            "{wire}"
        );
        let result = wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|block| block["type"] == "tool_result")
            .expect("successful Read must reach the provider as a tool result");
        assert!(result.get("is_error").is_none(), "{wire}");
        if expected_documents == 1 {
            let document = wire["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().into_iter().flatten())
                .find(|block| block["type"] == "document")
                .expect("the standalone PDF row must reach the provider");
            assert_eq!(
                document["source"],
                json!({
                    "type":"base64", "media_type":"application/pdf", "data":"JVBERi0xLjQK"
                }),
                "{wire}"
            );
        }
        if is_notebook {
            let content = &result["content"];
            assert_eq!(
                content[0]["text"],
                "<cell id=\"plot\">display(plot)🙂</cell id=\"plot\">\n\nplot output",
                "{wire}"
            );
            assert_eq!(content[1]["type"], "image", "{wire}");
            assert_eq!(
                content[1]["source"],
                json!({
                    "type":"base64", "media_type":"image/png", "data":"iVBORw0KGgo="
                }),
                "{wire}"
            );
        }
    }
}

#[test]
fn main_result_mapper_keeps_replacement_and_non_mcp_array_semantics() {
    use super::tool_dispatch::main_tool_result_content_blocks;
    let notebook = source(&json!({"type":"notebook", "file":{"cells":[{
        "cellType":"code", "cell_id":"plot", "source":"plot()", "outputs":[{
            "image":{"image_data":"iVBORw0KGgo=", "media_type":"image/png"}
        }]
    }]}}));
    let text = Utf16JsonProjection::from("hook replacement");
    assert!(
        main_tool_result_content_blocks("Read", &notebook, &text, false, true, false).is_none()
    );
    let array = source(&json!([{"type":"text", "text":"agent transcript"}]));
    assert!(main_tool_result_content_blocks("Agent", &array, &text, false, false, false).is_none());
    assert_eq!(
        main_tool_result_content_blocks("mcp__tool", &array, &text, true, false, false),
        Some(array)
    );
    let lone = Utf16JsonProjection::parse(r#"{"type":"notebook","file":{"cells":[{"cellType":"code","cell_id":"x","source":"\ud800"}]}}"#).unwrap();
    let blocks =
        main_tool_result_content_blocks("Read", &lone, &text, false, false, false).unwrap();
    let mut expected: Vec<u16> = "<cell id=\"x\">".encode_utf16().collect();
    expected.push(0xd800);
    expected.extend("</cell id=\"x\">".encode_utf16());
    assert_eq!(blocks.string_units("/0/text"), Some(expected));
}
