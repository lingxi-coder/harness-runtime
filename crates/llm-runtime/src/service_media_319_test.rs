use super::*;
use lingxi_core::types::{MessageId, ToolUseId};
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/media_limits_2_1_319.json")).unwrap()
}

fn parse_block(value: &Value, raw: bool) -> ContentBlock {
    if !raw {
        let mut typed = value.clone();
        if typed["type"] == "tool_result" && typed["content"].is_array() {
            typed["content_blocks"] = typed["content"].clone();
            typed["content"] = json!("");
        }
        if let Ok(block) = serde_json::from_value(typed) {
            return block;
        }
    }
    ContentBlock::ProviderContent {
        protocol: "anthropic_messages".into(),
        value: Utf16JsonProjection::parse(&value.to_string()).unwrap(),
    }
}

fn project_block(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::ProviderContent { value, .. } => {
            serde_json::from_str(&value.to_json_string().unwrap()).unwrap()
        }
        ContentBlock::ToolResult {
            content_blocks: Some(content),
            ..
        } => {
            let mut value = serde_json::to_value(block).unwrap();
            value["content"] = serde_json::from_str(&content.to_json_string().unwrap()).unwrap();
            value.as_object_mut().unwrap().remove("content_blocks");
            value
        }
        _ => serde_json::to_value(block).unwrap(),
    }
}

#[test]
fn media_projection_matches_native_319_source_vectors() {
    for case in fixture()["cases"].as_array().unwrap() {
        let input = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(row_index, row)| {
                let content = row["message"]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(block_index, block)| {
                        let raw = case["raw_blocks"].as_array().is_some_and(|indices| {
                            indices
                                .iter()
                                .any(|index| index == &json!([row_index, block_index]))
                        });
                        parse_block(block, raw)
                    })
                    .collect();
                let mut message = ConversationMessage::user(MessageId::new(), String::new());
                let ConversationMessage::User {
                    content: target, ..
                } = &mut message
                else {
                    unreachable!()
                };
                *target = content;
                message
            })
            .collect();
        let args = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap() as usize)
            .collect::<Vec<_>>();
        let limits = NativeMediaLimits {
            count: args[0],
            count_headroom: args[1],
            bytes: args[2],
            byte_headroom: args[3],
            document_bytes: args[4],
        };
        // Only explicit fixture host facts exercise the native private-source
        // rejection branch. The production entry point always supplies none.
        let rejected = case["host_rejected_media_indices"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|index| index.as_u64().unwrap() as usize)
            .collect::<Vec<_>>();
        let actual = strip_native_media_with_host_rejections(
            ConversationMessagesWithSources::new(input),
            limits,
            &rejected,
        );
        let projected = actual.messages.iter().map(|message| {
            let ConversationMessage::User { content, .. } = message else { unreachable!() };
            json!({ "type": "user", "message": { "role": "user", "content": content.iter().map(project_block).collect::<Vec<_>>() } })
        }).collect::<Vec<_>>();
        assert_eq!(json!(projected), case["expected"], "{}", case["name"]);
        for (message, sources) in actual.messages.iter().zip(actual.block_sources) {
            let ConversationMessage::User { content, .. } = message else {
                unreachable!()
            };
            assert_eq!(
                content.len(),
                sources.len(),
                "source coordinates survive: {}",
                case["name"]
            );
        }
    }
}

#[test]
fn media_model_and_endpoint_defaults_match_native_319() {
    let fixture = fixture();
    for policy in fixture["policies"].as_array().unwrap() {
        let limits = NativeMediaLimits::for_model(
            policy["model"].as_str().unwrap(),
            policy["disabled"].as_bool().unwrap(),
            policy["context_beta"].as_bool().unwrap(),
            false,
        );
        assert_eq!(
            limits.count as u64,
            policy["expected_count"].as_u64().unwrap(),
            "{policy}"
        );
    }
    for defaults in fixture["defaults"].as_array().unwrap() {
        let limits = NativeMediaLimits::for_model(
            "claude-opus-5",
            false,
            false,
            defaults["constrained_endpoint"].as_bool().unwrap(),
        );
        assert_eq!(limits.bytes as u64, defaults["bytes"].as_u64().unwrap());
        assert_eq!(
            limits.document_bytes as u64,
            defaults["document_bytes"].as_u64().unwrap()
        );
        assert_eq!(
            limits.count_headroom as u64,
            defaults["count_headroom"].as_u64().unwrap()
        );
        assert_eq!(
            limits.byte_headroom as u64,
            defaults["byte_headroom"].as_u64().unwrap()
        );
    }
}

#[test]
fn production_does_not_infer_host_rejection_from_source_text() {
    let mut message = ConversationMessage::user(MessageId::new(), String::new());
    let ConversationMessage::User { content, .. } = &mut message else {
        unreachable!()
    };
    content.push(parse_block(
        &json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "unsent:12"}
        }),
        false,
    ));
    let input = ConversationMessagesWithSources::new(vec![message]);
    let original = input.messages.clone();
    let limits = NativeMediaLimits {
        count: 100,
        count_headroom: 20,
        bytes: 100,
        byte_headroom: 0,
        document_bytes: 0,
    };
    let outgoing = strip_native_media_with_sources(input, limits);
    assert_eq!(
        outgoing.messages, original,
        "only an explicit host fact may reject a source"
    );
}

struct NeverTransport;
impl crate::test_support::FixtureTransport for NeverTransport {
    fn execute<'a>(
        &'a self,
        _: &'a crate::ProviderRequest,
    ) -> crate::BoxFuture<'a, Result<crate::ProviderResponse, LlmError>> {
        Box::pin(async { panic!("media request assembly never calls a provider") })
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a crate::ProviderRequest,
    ) -> crate::BoxFuture<'a, Result<crate::StreamingResponse, LlmError>> {
        Box::pin(async { panic!("media request assembly never calls a provider") })
    }
}
llm_runtime::impl_fixture_transport!(NeverTransport);

fn service(native: bool) -> ApiService {
    let mut provider = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|provider| provider.provider_id == crate::ProviderId::AnthropicFirstParty)
        .unwrap();
    provider.profile_name = "media-319".into();
    provider.auth = crate::AuthStrategy::None;
    provider.credential = crate::CredentialConfig::None;
    provider.wire_profile = None;
    provider.connection = Default::default();
    provider.models.truncate(1);
    let model = &mut provider.models[0];
    model.request_model = "claude-haiku-4-5".into();
    model.display_model = "claude-haiku-4-5".into();
    model.billing_model = "claude-haiku-4-5".into();
    model.aliases.clear();
    if !native {
        provider.provider_id = crate::ProviderId::Custom {
            name: "media-custom".into(),
        };
    }
    let client = ModelRuntime::from_config(crate::ClientConfig {
        providers: vec![provider],
    })
    .unwrap();
    ApiService::new(
        Arc::new(client),
        Arc::new(NeverTransport),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
    .with_thinking(crate::model::thinking::ThinkingConfig::Disabled)
}

fn images(count: usize) -> ConversationMessage {
    let mut message = ConversationMessage::user(MessageId::new(), String::new());
    let ConversationMessage::User { content, .. } = &mut message else {
        unreachable!()
    };
    *content = (0..count).map(|_| parse_block(&json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"YQ=="}}), false)).collect();
    message
}

fn request(service: &ApiService, messages: Vec<ConversationMessage>) -> LlmRequest {
    service
        .build_main_request(
            "claude-haiku-4-5",
            Some("media-319"),
            None,
            messages,
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Named("main"),
            None,
            &[],
            &Default::default(),
            &Default::default(),
        )
        .unwrap()
}

fn image_count(request: &LlmRequest) -> usize {
    request
        .input
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| {
            matches!(
                block,
                lingxi_llm_client::protocol::ContentBlock::Image { .. }
            )
        })
        .count()
}

#[test]
fn main_request_uses_native_count_headroom_and_preserves_other_provider_cap() {
    let native = request(&service(true), vec![images(101)]);
    assert_eq!(image_count(&native), 80);
    let custom = request(&service(false), vec![images(101)]);
    assert_eq!(image_count(&custom), 100);
}

#[test]
fn main_request_pairs_tools_before_counting_media() {
    let mut user = images(84);
    let ConversationMessage::User { content, .. } = &mut user else {
        unreachable!()
    };
    content.push(ContentBlock::ToolResult {
        tool_use_id: ToolUseId::from("orphan"), content: "".into(), is_error: None, provider_tool_use_id: None,
        content_blocks: Some(Utf16JsonProjection::parse(&Value::Array((0..30).map(|_| json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"YQ=="}})).collect()).to_string()).unwrap()),
    });
    let assistant = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "done".into(),
            citations: None,
        }],
        stop_reason: None,
        resumable: None,
        per_turn_effort: None,
    };
    let built = request(&service(true), vec![assistant, user]);
    assert_eq!(
        image_count(&built),
        84,
        "orphan tool-result media does not evict retained user images"
    );
}
