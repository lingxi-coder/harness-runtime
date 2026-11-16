//! Classifier prefix eligibility through the real selected-route preparation.
//! These tests assert reusable message boundaries, not invented cache counters.

use super::project_request;
use crate::{
    ClassifierCacheLayout, ClientConfig, Credential, CredentialProvider, CredentialScope, LlmError,
    LlmRequest, ModelRuntime, StaticCredentialProvider,
};
use lingxi_core::types::MessageId;
use lingxi_llm_client::{
    codecs::{openai::responses::OpenAiResponsesCodec, CodecContext, EncodeRequest, WireCodec},
    protocol::{
        AuthStrategy, CacheBreakpoint, CachePosition, CacheTtl, ContentBlock, ConversationMessage,
        JsonPayload, OpenAiPromptCacheMode, OpenAiPromptCacheOptions, OpenAiPromptCacheTtl,
        ProviderProfile, SystemBlock,
    },
    RequestMode,
};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MODEL: &str = "gpt-6.1-sol";
const PROFILE: &str = "classifier-prefix";
const KEY: &str = "synthetic-classifier-prefix-key";
const FIRST_ACTION: &str = "{\"Bash\":\"first fixture action\"}\n";
const SECOND_ACTION: &str = "{\"Bash\":\"second fixture action\"}\n";
const APPENDED_USER: &str = "{\"user\":\"new user turn while the prior query is pending\"}\n";

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.into(),
        thought_signature: None,
        citations: None,
    }
}

fn history(extended: bool) -> Vec<String> {
    let mut history = vec![
        format!(
            "{{\"user\":\"{}\"}}\n",
            "stable authorized context ".repeat(300)
        ),
        "{\"Write\":\"earlier completed fixture action\"}\n".into(),
    ];
    if extended {
        history.extend([
            FIRST_ACTION.into(),
            "{\"TaskUpdate\":\"locally allowed intervening step one\"}\n".into(),
            "{\"Write\":\"locally allowed intervening step two\"}\n".into(),
        ]);
    }
    history
}

fn classifier_request(
    stream: bool,
    history: &[String],
    action: &str,
    stage: &'static str,
) -> LlmRequest {
    let mut request = LlmRequest::new(MODEL).with_profile(PROFILE);
    request.stream = stream;
    request.input.max_tokens = Some(128);
    request
        .input
        .system
        .push(SystemBlock::new("Trusted classifier policy. ".repeat(100)));
    request
        .input
        .messages
        .push(ConversationMessage::user_text("quoted user configuration"));
    let mut blocks = vec![text("<transcript>\n")];
    blocks.extend(history.iter().map(|line| text(line)));
    let action_block = blocks.len();
    blocks.extend([
        text(action),
        text("</transcript>\n"),
        text(if stage == "fast" {
            "FAST: return the first judgment"
        } else {
            "THINKING: independently review the action"
        }),
    ]);
    request.input.messages.push(ConversationMessage {
        role: lingxi_llm_client::protocol::MessageRole::User,
        content: blocks,
        native_options: Vec::new(),
    });
    request.execution.classifier_cache_layout = Some(ClassifierCacheLayout {
        logical_id: format!("fixture-{stage}"),
        stage,
        history: Some(CachePosition::Message {
            index: 1,
            block: action_block - 1,
        }),
        action: Some(CachePosition::Message {
            index: 1,
            block: action_block,
        }),
    });
    request.execution.request_session_id = Some("classifier-prefix-parent".into());
    request.execution.input_protocol =
        Some(lingxi_llm_client::protocol::ProtocolFamily::OpenAiResponses);
    request
}

fn runtime_with(credentials: Arc<dyn CredentialProvider>) -> ModelRuntime {
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"open_ai", "profile_name":PROFILE,
        "base_url":"https://api.openai.com/v1", "protocol":"open_ai_responses",
        "auth":"api_key", "credential":{"type":"host_managed","id":"classifier-prefix-fixture"},
        "models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,
            "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":true}}]
    }]})).unwrap();
    ModelRuntime::from_config(config)
        .unwrap()
        .with_credential_provider(credentials)
}

fn fresh_runtime() -> ModelRuntime {
    runtime_with(Arc::new(StaticCredentialProvider::new(Credential::ApiKey(
        KEY.into(),
    ))))
}

async fn body(runtime: &ModelRuntime, request: &LlmRequest) -> Value {
    let prepared = runtime
        .prepare(request)
        .await
        .expect("SDK preparation succeeds without dispatch");
    serde_json::from_slice(&prepared.provider_request.wire_body_bytes().unwrap()).unwrap()
}

fn without_markers(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(without_markers).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .filter(|(name, _)| name.as_str() != "prompt_cache_breakpoint")
                .map(|(name, value)| (name.clone(), without_markers(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn eligible_message_ending_at(body: &Value, line: &str) -> Option<usize> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .position(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_array()
                    .and_then(|parts| parts.last())
                    .is_some_and(|part| part["text"] == line)
        })
}

fn message_ending_at(body: &Value, line: &str) -> usize {
    eligible_message_ending_at(body, line)
        .expect("the old cached action must remain a complete user-message ending")
}

fn old_composite_wire(request: &LlmRequest) -> Value {
    let mode = if request.stream {
        RequestMode::Stream
    } else {
        RequestMode::Complete
    };
    let profile = official_profile();
    let mut input = crate::upstream::request(request, profile.protocol).unwrap();
    super::project(
        &mut input,
        &profile,
        MODEL,
        AuthStrategy::ApiKey,
        mode,
        request.execution.classifier_cache_layout.as_ref().unwrap(),
        request.execution.request_session_id.as_deref(),
        true,
    );
    let wire = OpenAiResponsesCodec
        .encode_request(
            EncodeRequest::new(&input),
            &CodecContext::new(&profile, MODEL, mode),
        )
        .unwrap();
    serde_json::from_slice(&wire.body).unwrap()
}

fn explicitly_marked_line(body: &Value, line: &str) -> bool {
    body["input"].as_array().unwrap().iter().any(|message| {
        message["content"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["text"] == line && part.get("prompt_cache_breakpoint").is_some())
        })
    })
}

fn shared_eligible_prefix(first: &Value, next: &Value, line: &str) {
    assert_eq!(first["prompt_cache_options"]["mode"], "implicit");
    assert_eq!(next["prompt_cache_options"]["mode"], "implicit");
    let old_end = message_ending_at(first, line);
    let new_end = message_ending_at(next, line);
    let old = first["input"].as_array().unwrap();
    let new = next["input"].as_array().unwrap();
    // OpenAI's implicit lookup includes the latest eligible message ending and
    // up to twenty earlier eligible endings. Keep this fixture inside that cap.
    let eligible_after = new[new_end + 1..]
        .iter()
        .filter(|item| item["role"] == "user")
        .count();
    assert!(
        eligible_after <= 20,
        "prior action remains in the documented lookup window"
    );
    assert_eq!(
        without_markers(&Value::Array(old[..=old_end].to_vec())),
        without_markers(&Value::Array(new[..=new_end].to_vec())),
        "the eligible prefix is byte-equivalent after removing cache control metadata",
    );
}

#[tokio::test]
async fn complete_and_stream_keep_previous_action_eligible_after_multiple_local_steps() {
    for stream in [false, true] {
        let runtime = fresh_runtime();
        let first = classifier_request(stream, &history(false), FIRST_ACTION, "fast");
        let next = classifier_request(stream, &history(true), SECOND_ACTION, "fast");
        let original_first = first.input.clone();
        let original_next = next.input.clone();
        // Reproduce the pre-fix physical topology with the same SDK encoder.
        // A1 was explicitly written, but no longer appears among the next
        // request's explicit points or eligible message endings.
        let old_composite = old_composite_wire(&first);
        let next_composite = old_composite_wire(&next);
        assert!(explicitly_marked_line(&old_composite, FIRST_ACTION));
        assert!(!explicitly_marked_line(&next_composite, FIRST_ACTION));
        assert!(eligible_message_ending_at(&next_composite, FIRST_ACTION).is_none());
        let old_wire = body(&runtime, &first).await;
        let next_wire = body(&runtime, &next).await;
        shared_eligible_prefix(&old_wire, &next_wire, FIRST_ACTION);
        assert_eq!(old_wire["stream"].as_bool().unwrap_or(false), stream);
        assert_eq!(next_wire["stream"].as_bool().unwrap_or(false), stream);
        assert!(old_wire.get("previous_response_id").is_none());
        assert!(next_wire.get("previous_response_id").is_none());
        assert_eq!(first.input, original_first);
        assert_eq!(next.input, original_next);
    }
}

#[tokio::test]
async fn preparing_openai_does_not_split_the_canonical_anthropic_fallback_wire() {
    for stream in [false, true] {
        let config: ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"classifier-fallback",
            "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages",
            "auth":"api_key", "credential":{"type":"host_managed","id":"classifier-prefix-fixture"},
            "models":[{"display_model":MODEL,"request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6",
                "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":true}}]
        }]})).unwrap();
        let fallback_runtime = ModelRuntime::from_config(config)
            .unwrap()
            .with_credential_provider(Arc::new(StaticCredentialProvider::new(Credential::ApiKey(
                KEY.into(),
            ))));
        let mut request = classifier_request(stream, &history(true), SECOND_ACTION, "fast");
        let canonical = request.input.clone();
        let openai = body(&fresh_runtime(), &request).await;
        assert!(openai["input"].as_array().unwrap().len() > 3);
        request.profile = Some("classifier-fallback".into());
        let fallback = body(&fallback_runtime, &request).await;
        assert_eq!(
            fallback["messages"].as_array().unwrap().len(),
            canonical.messages.len()
        );
        assert_eq!(
            fallback["messages"][1]["content"].as_array().unwrap().len(),
            canonical.messages[1].content.len()
        );
        assert_eq!(request.input, canonical);
    }
}

#[tokio::test]
async fn fast_and_thinking_share_action_prefix_without_chaining_the_first_verdict() {
    for stream in [false, true] {
        let runtime = fresh_runtime();
        let fast = classifier_request(stream, &history(true), SECOND_ACTION, "fast");
        let thinking = classifier_request(stream, &history(true), SECOND_ACTION, "thinking");
        let first_wire = body(&runtime, &fast).await;
        let second_wire = body(&runtime, &thinking).await;
        shared_eligible_prefix(&first_wire, &second_wire, SECOND_ACTION);
        assert_ne!(
            first_wire["input"], second_wire["input"],
            "stage suffixes stay distinct"
        );
        for wire in [&first_wire, &second_wire] {
            assert!(wire.get("previous_response_id").is_none());
            assert!(wire["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "assistant"));
        }
    }
}

fn sdk_profile(provider: &str, protocol: &str, endpoint: &str, model: &str) -> ProviderProfile {
    serde_json::from_value(json!({
        "provider_id":provider, "profile_name":PROFILE, "base_url":endpoint,
        "protocol":protocol, "auth":"none",
        "models":[{"display_model":model,"request_model":model,"billing_model":model}]
    }))
    .unwrap()
}

fn official_profile() -> ProviderProfile {
    sdk_profile(
        "openai",
        "open_ai_responses",
        "https://api.openai.com/v1",
        MODEL,
    )
}

#[test]
fn classifier_split_remaps_exact_text_cache_positions_and_source_authority_together() {
    let mut request = classifier_request(false, &history(false), FIRST_ACTION, "fast");
    let units = vec!['h' as u16, 0xd800, 'x' as u16];
    request.input.messages[1].content[1] = text(&String::from_utf16_lossy(&units));
    request
        .input
        .messages
        .push(ConversationMessage::user_text("following message"));
    request
        .execution
        .message_json_string_overrides
        .insert("/messages/1/content/1/text".into(), units.clone());
    request.execution.message_json_string_overrides.insert(
        "/messages/2/content/0/text".into(),
        "following message".encode_utf16().collect(),
    );
    request
        .execution
        .message_json_string_overrides
        .insert("/system/0/text".into(), vec![42]);
    request.input.prompt_cache.prompt_cache_key = Some("caller-selected-cache-key".into());
    request.input.prompt_cache.prompt_cache_options = Some(OpenAiPromptCacheOptions {
        mode: Some(OpenAiPromptCacheMode::Implicit),
        ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
    });
    request.input.prompt_cache.breakpoints = vec![
        CacheBreakpoint {
            position: CachePosition::System { index: 0 },
            ttl: CacheTtl::ThirtyMinutes,
            scope: None,
        },
        CacheBreakpoint {
            position: CachePosition::Message { index: 1, block: 1 },
            ttl: CacheTtl::ThirtyMinutes,
            scope: None,
        },
        CacheBreakpoint {
            position: CachePosition::Message { index: 2, block: 0 },
            ttl: CacheTtl::ThirtyMinutes,
            scope: None,
        },
    ];
    let ids = request
        .input
        .messages
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .map(|_| HashSet::from([MessageId::new()]))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    request.execution.request_content_source_ids = ids.clone();
    let trailing_source = MessageId::new();
    request
        .execution
        .request_message_source_indices
        .insert(trailing_source, 2);
    let original = request.clone();
    let projected = project_request(
        &request,
        &official_profile(),
        MODEL,
        AuthStrategy::ApiKey,
        RequestMode::Complete,
    );
    assert!(matches!(projected, Cow::Owned(_)));
    let target_len = request.input.messages[1].content.len();
    assert_eq!(projected.input.messages.len(), target_len + 2);
    assert_eq!(
        projected.execution.message_json_string_overrides["/messages/2/content/0/text"],
        units
    );
    assert_eq!(
        projected.execution.message_json_string_overrides
            [&format!("/messages/{}/content/0/text", target_len + 1)],
        "following message".encode_utf16().collect::<Vec<_>>()
    );
    assert_eq!(
        projected.execution.message_json_string_overrides["/system/0/text"],
        vec![42]
    );
    assert_eq!(
        projected.input.prompt_cache.prompt_cache_key,
        original.input.prompt_cache.prompt_cache_key
    );
    assert_eq!(
        projected.input.prompt_cache.prompt_cache_options,
        original.input.prompt_cache.prompt_cache_options
    );
    assert_eq!(
        projected.input.prompt_cache.breakpoints[1].position,
        CachePosition::Message { index: 2, block: 0 }
    );
    assert_eq!(
        projected.input.prompt_cache.breakpoints[2].position,
        CachePosition::Message {
            index: target_len + 1,
            block: 0
        }
    );
    assert_eq!(projected.execution.request_content_source_ids[0], ids[0]);
    for (block, sources) in ids[1].iter().enumerate() {
        assert_eq!(
            projected.execution.request_content_source_ids[block + 1],
            vec![sources.clone()]
        );
    }
    assert_eq!(
        projected.execution.request_content_source_ids[target_len + 1],
        ids[2]
    );
    assert_eq!(
        projected.execution.request_message_source_indices[&trailing_source],
        target_len + 1
    );
    let layout = projected
        .execution
        .classifier_cache_layout
        .as_ref()
        .unwrap();
    assert_eq!(
        layout.history,
        Some(CachePosition::Message { index: 3, block: 0 })
    );
    assert_eq!(
        layout.action,
        Some(CachePosition::Message { index: 4, block: 0 })
    );
    assert_eq!(request.input, original.input);
    assert_eq!(
        request.execution.message_json_string_overrides,
        original.execution.message_json_string_overrides
    );
    assert_eq!(
        request.execution.request_content_source_ids,
        original.execution.request_content_source_ids
    );
    assert!(
        matches!(
            project_request(
                projected.as_ref(),
                &official_profile(),
                MODEL,
                AuthStrategy::ApiKey,
                RequestMode::Complete
            ),
            Cow::Borrowed(_)
        ),
        "re-preparing a projected request does not split it again"
    );
}

#[tokio::test]
async fn complete_and_stream_encode_lone_surrogates_at_remapped_physical_locations() {
    for stream in [false, true] {
        let runtime = fresh_runtime();
        let mut request = classifier_request(stream, &history(false), FIRST_ACTION, "fast");
        let first_units = vec!['a' as u16, 0xd800, 'b' as u16];
        let following_units = vec!['c' as u16, 0xdfff, 'd' as u16];
        request.input.messages[1].content[1] = text(&String::from_utf16_lossy(&first_units));
        request
            .input
            .messages
            .push(ConversationMessage::user_text(String::from_utf16_lossy(
                &following_units,
            )));
        request.execution.message_json_string_overrides = BTreeMap::from([
            ("/messages/1/content/1/text".into(), first_units.clone()),
            ("/messages/2/content/0/text".into(), following_units.clone()),
        ]);
        let prepared = runtime.prepare(&request).await.unwrap();
        let source =
            String::from_utf8(prepared.provider_request.wire_body_bytes().unwrap()).unwrap();
        let payload = JsonPayload::parse(&source).unwrap();
        let input_len = payload.array_len("/input").unwrap();
        let strings = (0..input_len)
            .filter_map(|index| {
                payload
                    .payload_at(&format!("/input/{index}/content/0/text"))?
                    .root_string_code_units()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            strings
                .iter()
                .filter(|units| **units == first_units)
                .count(),
            1
        );
        assert_eq!(
            strings
                .iter()
                .filter(|units| **units == following_units)
                .count(),
            1
        );
        assert!(source.contains("\\ud800"));
        assert!(source.contains("\\udfff"));
    }
}

#[derive(Debug, Default)]
struct CancelSecondCredentialLoad {
    calls: AtomicUsize,
    blocked: tokio::sync::Notify,
}

impl CredentialProvider for CancelSecondCredentialLoad {
    fn load<'a>(
        &'a self,
        _: &'a CredentialScope,
    ) -> crate::BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.blocked.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(Credential::ApiKey(KEY.into()))
        })
    }
}

#[tokio::test]
async fn canceled_prepare_continues_with_appended_user_and_cold_rebuild_keeps_prefix() {
    for stream in [false, true] {
        let credentials = Arc::new(CancelSecondCredentialLoad::default());
        let runtime = Arc::new(runtime_with(credentials.clone()));
        let first = classifier_request(stream, &history(false), FIRST_ACTION, "fast");
        let first_wire = body(&runtime, &first).await;
        let pending = classifier_request(stream, &history(true), SECOND_ACTION, "fast");
        let pending_input = pending.input.clone();
        let task_runtime = runtime.clone();
        let task_request = pending.clone();
        let task = tokio::spawn(async move { task_runtime.prepare(&task_request).await });
        tokio::time::timeout(Duration::from_secs(5), credentials.blocked.notified())
            .await
            .unwrap();
        let mut appended = history(true);
        appended.push(APPENDED_USER.into());
        let continued = classifier_request(stream, &appended, SECOND_ACTION, "fast");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let continued_wire = body(&runtime, &continued).await;
        shared_eligible_prefix(&first_wire, &continued_wire, FIRST_ACTION);
        assert_eq!(
            continued_wire["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["content"][0]["text"] == APPENDED_USER)
                .count(),
            1
        );
        assert_eq!(
            pending.input, pending_input,
            "cancellation must not consume canonical history"
        );

        // Drop a prepared draft without dispatch, then independently reconstruct
        // the canonical input as cold recovery does, with no projection state.
        drop(runtime.prepare(&continued).await.unwrap());
        let input_json = serde_json::to_string(&continued.input).unwrap();
        let mut cold_request = classifier_request(stream, &appended, SECOND_ACTION, "fast");
        cold_request.input = serde_json::from_str(&input_json).unwrap();
        let cold_wire = body(&fresh_runtime(), &cold_request).await;
        assert_eq!(
            continued_wire, cold_wire,
            "hot replay and cold reconstruction prepare identical inputs"
        );
        shared_eligible_prefix(&first_wire, &cold_wire, FIRST_ACTION);
    }
}

#[test]
fn unsupported_routes_and_untrusted_or_inconsistent_layouts_remain_unchanged() {
    let request = classifier_request(false, &history(false), FIRST_ACTION, "fast");
    let cases = [
        (
            sdk_profile(
                "openai",
                "open_ai_responses",
                "https://api.openai.com/v1",
                "gpt-5.4",
            ),
            "gpt-5.4",
            AuthStrategy::ApiKey,
        ),
        (
            sdk_profile(
                "openai",
                "open_ai_responses",
                "https://compatible.test/v1",
                MODEL,
            ),
            MODEL,
            AuthStrategy::ApiKey,
        ),
        (
            sdk_profile(
                "openai",
                "open_ai_responses",
                "https://chatgpt.com/backend-api/codex",
                MODEL,
            ),
            MODEL,
            AuthStrategy::OAuthBearer,
        ),
        (
            sdk_profile(
                "anthropic",
                "anthropic_messages",
                "https://api.anthropic.com",
                "claude-sonnet-4-6",
            ),
            "claude-sonnet-4-6",
            AuthStrategy::ApiKey,
        ),
        (official_profile(), MODEL, AuthStrategy::None),
    ];
    for mode in [RequestMode::Complete, RequestMode::Stream] {
        for (profile, model, auth) in &cases {
            assert!(matches!(
                project_request(&request, profile, model, *auth, mode),
                Cow::Borrowed(_)
            ));
        }
        let mut disabled = request.clone();
        disabled.execution.enable_prompt_caching = Some(false);
        assert!(matches!(
            project_request(
                &disabled,
                &official_profile(),
                MODEL,
                AuthStrategy::ApiKey,
                mode
            ),
            Cow::Borrowed(_)
        ));
        let mut ordinary = request.clone();
        ordinary.execution.classifier_cache_layout = None;
        assert!(matches!(
            project_request(
                &ordinary,
                &official_profile(),
                MODEL,
                AuthStrategy::ApiKey,
                mode
            ),
            Cow::Borrowed(_)
        ));
        let mut inconsistent = request.clone();
        inconsistent.execution.request_content_source_ids = vec![Vec::new()];
        assert!(matches!(
            project_request(
                &inconsistent,
                &official_profile(),
                MODEL,
                AuthStrategy::ApiKey,
                mode
            ),
            Cow::Borrowed(_)
        ));
        let mut invalid_override = request.clone();
        invalid_override
            .execution
            .message_json_string_overrides
            .insert("/messages/99/content/0/text".into(), vec![0xd800]);
        assert!(matches!(
            project_request(
                &invalid_override,
                &official_profile(),
                MODEL,
                AuthStrategy::ApiKey,
                mode
            ),
            Cow::Borrowed(_)
        ));
    }
    // Selecting a different protocol never observes the route-local split.
    let split = project_request(
        &request,
        &official_profile(),
        MODEL,
        AuthStrategy::ApiKey,
        RequestMode::Complete,
    );
    assert!(split.input.messages.len() > request.input.messages.len());
    let anthropic = sdk_profile(
        "anthropic",
        "anthropic_messages",
        "https://api.anthropic.com",
        "claude-sonnet-4-6",
    );
    let fallback = project_request(
        &request,
        &anthropic,
        "claude-sonnet-4-6",
        AuthStrategy::ApiKey,
        RequestMode::Complete,
    );
    assert!(matches!(fallback, Cow::Borrowed(_)));
    assert_eq!(fallback.input, request.input);
    assert_eq!(request.input.messages.len(), 2);
}
