//! Physical accounting must describe the same input as generation. Everything
//! uses the production SDK/ApiService path and a local transport, not a tokenizer.

use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::host::{ContextMetadata, ContextPolicy, ModelMetadata};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use llm_runtime::services::sdk;
use llm_runtime::{
    ApiService, ClientConfig, LlmRequest, ModelRuntime, ProtocolFamily, SubscriberState,
};
use sdk::providers::anthropic::request_policy::AnthropicRequestKind;
use serde_json::json;
use std::sync::{Arc, Mutex};

const MODEL: &str = "claude-sonnet-4-6";
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn run(future: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            let names = [
                "CLAUDE_CODE_EXTRA_BODY",
                branding::EFFORT_LEVEL_ENV,
                branding::DISABLE_STRUCTURED_OUTPUTS_ENV,
                "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            ];
            let _restore = EnvRestore(
                names
                    .iter()
                    .map(|&name| (name, std::env::var_os(name)))
                    .collect(),
            );
            for name in names {
                std::env::remove_var(name);
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[derive(Clone)]
struct Observation {
    url: String,
    count: bool,
    body: sdk::protocol::JsonPayload,
}

#[derive(Default)]
struct LocalTransport {
    calls: Mutex<Vec<Observation>>,
    replace_env_on_count: Mutex<Option<String>>,
    fail_primary: bool,
    count_status: Option<u16>,
}

#[async_trait]
impl llm_runtime::Transport for LocalTransport {
    async fn send(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        let body =
            sdk::protocol::JsonPayload::parse(std::str::from_utf8(&request.body).unwrap()).unwrap();
        let count = request.url.contains("/count_tokens");
        let stream = body.bool_at("/stream") == Some(true);
        let responses = request.url.ends_with("/responses");
        let model = body.string_at("/model").unwrap_or_else(|| MODEL.into());
        let primary = self.fail_primary && request.url.contains("primary.example.invalid");
        self.calls.lock().unwrap().push(Observation {
            url: request.url,
            count,
            body,
        });
        if count {
            if let Some(next) = self.replace_env_on_count.lock().unwrap().take() {
                std::env::set_var("CLAUDE_CODE_EXTRA_BODY", next);
            }
            tokio::task::yield_now().await;
        }
        let (status, content_type, bytes) = if primary {
            (
                503,
                "application/json",
                serde_json::to_vec(&json!({"error":{"message":"retry next connection"}})).unwrap(),
            )
        } else if count && self.count_status.is_some_and(|status| status != 200) {
            (
                self.count_status.unwrap(),
                "application/json",
                serde_json::to_vec(&json!({"type":"error","error":{"type":"invalid_request_error","message":"CountTokens endpoint unavailable"}})).unwrap(),
            )
        } else if count {
            (
                200,
                "application/json",
                serde_json::to_vec(&json!({"input_tokens":100})).unwrap(),
            )
        } else if responses {
            let response = json!({"id":"response_fixture","model":model,"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"{}","annotations":[]}]}],"usage":{"input_tokens":100,"output_tokens":1,"total_tokens":101}});
            if stream {
                let frames = [
                    json!({"type":"response.created","response":{"id":"response_fixture","model":model}}),
                    json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"{}"}),
                    json!({"type":"response.completed","response":response}),
                ];
                (
                    200,
                    "text/event-stream",
                    frames
                        .iter()
                        .map(|frame| format!("data: {frame}\n\n"))
                        .collect::<String>()
                        .into_bytes(),
                )
            } else {
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&response).unwrap(),
                )
            }
        } else if stream {
            let frames = [
                json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":100,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"{}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ];
            let source = frames
                .iter()
                .map(|frame| {
                    format!(
                        "event: {}\ndata: {frame}\n\n",
                        frame["type"].as_str().unwrap()
                    )
                })
                .collect::<String>();
            (200, "text/event-stream", source.into_bytes())
        } else {
            (200, "application/json", serde_json::to_vec(&json!({"id":"fixture","type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"{}"}],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":1}})).unwrap())
        };
        Ok(sdk::StreamResponse {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: futures::stream::once(async move { Ok(bytes.into()) }).boxed(),
        })
    }
}

fn profile(name: &str, protocol: ProtocolFamily, budget: u64) -> llm_runtime::ProviderProfile {
    let mut profile = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "anthropic")
        .unwrap();
    profile.profile_name = name.into();
    profile.provider_id = llm_runtime::ProviderId::Custom {
        name: "accounting".into(),
    };
    profile.protocol = protocol;
    profile.base_url = format!("https://{name}.example.invalid");
    profile.wire_profile = None;
    profile.connection = Default::default();
    profile.auth = llm_runtime::AuthStrategy::None;
    profile.credential = llm_runtime::CredentialConfig::None;
    profile.models.truncate(1);
    let model = &mut profile.models[0];
    model.display_model = MODEL.into();
    model.request_model = MODEL.into();
    model.billing_model = MODEL.into();
    model.aliases.clear();
    model.capabilities = llm_runtime::Capabilities {
        streaming: true,
        tools: true,
        reasoning: true,
        structured_output: true,
        ..Default::default()
    };
    model.metadata = ModelMetadata {
        context_window_tokens: Some(1_000_000),
        context: Some(ContextMetadata {
            application: Some(ContextPolicy::Custom {
                input_tokens: budget,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    profile
}

fn api(runtime: Arc<ModelRuntime>, transport: Arc<LocalTransport>) -> ApiService {
    ApiService::new(
        runtime,
        transport,
        SubscriberState::default(),
        llm_runtime::model::user_agent::UserAgentEnv::default(),
        "fixture",
        None,
        None,
    )
    .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled)
}

async fn execute(api: &ApiService, request: LlmRequest, stream: bool) {
    if stream {
        let mut events = api.stream_request(request).await.unwrap();
        while let Some(event) = events.next().await {
            event.unwrap();
        }
    } else {
        api.execute_non_stream_request(
            request,
            llm_runtime::NonStreamingRequestClass::Main,
            Default::default(),
        )
        .await
        .unwrap();
    }
}

fn large_schema_request(profile: &str) -> LlmRequest {
    let mut request = LlmRequest::new(MODEL)
        .with_profile(profile)
        .with_user_text("hello");
    request.input.max_tokens = Some(64);
    request.input.output_format = sdk::protocol::OutputFormat::JsonSchema {
        name: "result".into(),
        strict: true,
        schema: sdk::protocol::JsonPayload::from_generated_value(&json!({
            "type":"object", "description":"schema detail ".repeat(1000),
            "properties":{}, "additionalProperties":false,
        }))
        .unwrap(),
    };
    request
}

#[test]
fn suppressed_schema_uses_effective_input_when_count_endpoint_is_unavailable_in_both_flows() {
    run(async {
        const BUDGET: u64 = 1000;
        for status in [404, 405, 501] {
            let mut provider = profile("target", ProtocolFamily::AnthropicMessages, BUDGET);
            provider.models[0].capabilities.structured_output = false;
            let runtime = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![provider],
                })
                .unwrap(),
            );
            let request = large_schema_request("target");
            let mut text = request.clone();
            text.input.output_format = sdk::protocol::OutputFormat::Text;
            assert!(llm_runtime::model::count_tokens::approximate_tokens(&request) > BUDGET);
            for body in [
                runtime
                    .prepare(&request)
                    .await
                    .unwrap()
                    .provider_request
                    .body_json,
                runtime
                    .prepare_count_tokens(&request)
                    .await
                    .unwrap()
                    .body_json,
            ] {
                assert!(body.payload_at("/output_config/format").is_none());
            }
            let counter = Arc::new(LocalTransport {
                count_status: Some(status),
                ..Default::default()
            });
            let count =
                llm_runtime::model::count_tokens::count_tokens(&runtime, counter.clone(), &request)
                    .await
                    .unwrap();
            assert_eq!(
                count,
                llm_runtime::model::count_tokens::count_tokens(&runtime, counter.clone(), &text)
                    .await
                    .unwrap(),
                "{status}: the same effective input has the same fallback estimate"
            );
            assert!(count < BUDGET);
            assert!(counter.calls.lock().unwrap().iter().all(|call| {
                call.count && call.body.payload_at("/output_config/format").is_none()
            }));
            for stream in [false, true] {
                let transport = Arc::new(LocalTransport {
                    count_status: Some(status),
                    ..Default::default()
                });
                execute(
                    &api(runtime.clone(), transport.clone()),
                    request.clone(),
                    stream,
                )
                .await;
                let calls = transport.calls.lock().unwrap();
                assert_eq!(
                    calls.len(),
                    1,
                    "{status}/{stream}: small input dispatches directly"
                );
                assert!(!calls[0].count);
                assert_eq!(calls[0].body.bool_at("/stream") == Some(true), stream);
                assert!(calls[0].body.payload_at("/output_config/format").is_none());
            }
            assert!(matches!(
                request.input.output_format,
                sdk::protocol::OutputFormat::JsonSchema { .. }
            ));
        }
    });
}

#[test]
fn supported_schema_remains_in_fallback_budget_and_is_rejected_in_both_flows() {
    run(async {
        const BUDGET: u64 = 1000;
        for status in [404, 405, 501] {
            let runtime = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![profile("target", ProtocolFamily::AnthropicMessages, BUDGET)],
                })
                .unwrap(),
            );
            let request = large_schema_request("target");
            let prepared = runtime.prepare(&request).await.unwrap();
            let count = runtime.prepare_count_tokens(&request).await.unwrap();
            let generation_format = prepared
                .provider_request
                .body_json
                .payload_at("/output_config/format")
                .unwrap();
            assert!(generation_format
                .semantically_eq(&count.body_json.payload_at("/output_config/format").unwrap())
                .unwrap());
            let estimated = llm_runtime::model::count_tokens::count_tokens(
                &runtime,
                Arc::new(LocalTransport {
                    count_status: Some(status),
                    ..Default::default()
                }),
                &request,
            )
            .await
            .unwrap();
            assert!(estimated > BUDGET);
            for stream in [false, true] {
                let transport = Arc::new(LocalTransport {
                    count_status: Some(status),
                    ..Default::default()
                });
                let service = api(runtime.clone(), transport.clone());
                let result = if stream {
                    async {
                        let mut events = service.stream_request(request.clone()).await?;
                        while let Some(event) = events.next().await {
                            event?;
                        }
                        Ok::<(), llm_runtime::LlmError>(())
                    }
                    .await
                } else {
                    service
                        .execute_non_stream_request(
                            request.clone(),
                            llm_runtime::NonStreamingRequestClass::Main,
                            Default::default(),
                        )
                        .await
                        .map(|_| ())
                };
                assert!(
                    matches!(result, Err(llm_runtime::LlmError::InputBudgetExceeded {
                    input_tokens, budget: BUDGET, estimated: true,
                }) if input_tokens == estimated),
                    "{status}/{stream}: {result:?}"
                );
                let calls = transport.calls.lock().unwrap();
                assert_eq!(calls.len(), 1);
                assert!(
                    calls[0].count,
                    "no generation when the retained schema exceeds the budget"
                );
                assert!(calls[0]
                    .body
                    .payload_at("/output_config/format")
                    .unwrap()
                    .semantically_eq(&generation_format)
                    .unwrap());
            }
        }
    });
}

#[test]
fn schema_suppression_does_not_remove_supported_fallback_schema_in_both_flows() {
    run(async {
        for stream in [false, true] {
            let mut primary = profile("primary", ProtocolFamily::AnthropicMessages, 1000);
            primary.models[0].capabilities.structured_output = false;
            let mut target = profile("target", ProtocolFamily::AnthropicMessages, 4000);
            for (order, provider) in [&mut primary, &mut target].into_iter().enumerate() {
                provider.connection.group = Some("schema-fallback".into());
                provider.connection.connection_id = Some(provider.profile_name.clone());
                provider.connection.order = order as u32;
                provider.connection.failover = llm_runtime::FailoverTriggers::DEFAULT;
            }
            let runtime = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![primary, target],
                })
                .unwrap(),
            );
            let transport = Arc::new(LocalTransport {
                fail_primary: true,
                ..Default::default()
            });
            let request = large_schema_request("primary");
            let expected = runtime
                .prepare_count_tokens(&large_schema_request("target"))
                .await
                .unwrap()
                .body_json
                .payload_at("/output_config/format")
                .unwrap();
            execute(&api(runtime, transport.clone()), request, stream).await;
            let calls = transport.calls.lock().unwrap();
            assert_eq!(
                calls.len(),
                3,
                "primary generation, target exact count, target generation"
            );
            assert!(!calls[0].count && calls[0].url.contains("primary.example.invalid"));
            assert!(calls[0].body.payload_at("/output_config/format").is_none());
            assert!(calls[1].count && !calls[2].count);
            for call in &calls[1..] {
                assert!(call.url.contains("target.example.invalid"));
                assert!(call
                    .body
                    .payload_at("/output_config/format")
                    .unwrap()
                    .semantically_eq(&expected)
                    .unwrap());
            }
        }
    });
}

#[test]
fn near_budget_cross_protocol_failover_projects_count_and_generation_for_both_drivers() {
    run(async {
        for stream in [false, true] {
            let mut primary = profile("primary", ProtocolFamily::OpenAiResponses, 1_000_000);
            let mut target = profile("target", ProtocolFamily::AnthropicMessages, 1000);
            for (order, profile) in [&mut primary, &mut target].into_iter().enumerate() {
                profile.connection.group = Some("accounting".into());
                profile.connection.connection_id = Some(profile.profile_name.clone());
                profile.connection.order = order as u32;
                profile.connection.failover = llm_runtime::FailoverTriggers::DEFAULT;
            }
            let runtime = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![primary, target],
                })
                .unwrap(),
            );
            let transport = Arc::new(LocalTransport {
                fail_primary: true,
                ..Default::default()
            });
            let service = api(runtime, transport.clone());
            let mut exact_units: Vec<u16> = "continue ".repeat(320).encode_utf16().collect();
            exact_units.extend("😀".encode_utf16());
            exact_units.extend_from_slice(&[0xd800, u16::from(b'/'), 0xdc00]);
            let mut generation_units = exact_units.clone();
            // Native VNe normalizes malformed value strings only when the
            // Claude generation is sealed. Draft/count input stays exact.
            let len = generation_units.len();
            generation_units[len - 3] = 0xfffd;
            generation_units[len - 1] = 0xfffd;
            let mut request = llm_runtime::MessagesCreateRequest::new(MODEL, Some("primary"), None, vec![
                ConversationMessage::user(MessageId::new(), "first".into()),
                ConversationMessage::Assistant { id:MessageId::new(), content:vec![ContentBlock::ProviderContent { protocol:"open_ai_responses".into(), value:Utf16JsonProjection::from_serde_value(&json!({"type":"reasoning","id":"r","summary":[],"encrypted_content":"A".repeat(60_000)})).unwrap() }], stop_reason:Some("end_turn".into()), per_turn_effort:None, resumable:None },
                ConversationMessage::User { id:MessageId::new(), content:vec![ContentBlock::TextJsUtf16 { text:String::from_utf16_lossy(&exact_units), utf16_code_units:exact_units.clone(), citations:None }], is_meta:false, is_compact_summary:false, is_visible_in_transcript_only:false, turn_companion:None, usage_limit_note:None, origin:None, turn_origin:None, api_message_override:None, interrupted_by_shutdown:None, refused_message_id:None },
            ], vec![]);
            request.opts.max_output_tokens = Some(64);
            if stream {
                service
                    .messages_create_buffered_stream(request)
                    .await
                    .unwrap();
            } else {
                service.messages_create(request).await.unwrap();
            }
            let calls = transport.calls.lock().unwrap();
            assert_eq!(
                calls.len(),
                3,
                "primary, target counter and target generation"
            );
            assert!(calls[0].url.contains("primary.example.invalid"));
            assert_eq!(
                calls[0]
                    .body
                    .payload_at("/input/2/content/0/text")
                    .unwrap()
                    .root_string_code_units(),
                Some(exact_units.clone())
            );
            assert!(calls[1].count && !calls[2].count);
            for (call, expected) in [(&calls[1], &exact_units), (&calls[2], &generation_units)] {
                assert!(!call.body.as_json_source().contains("encrypted_content"));
                let messages = call.body.payload_at("/messages").unwrap();
                assert_eq!(messages.array_len(""), Some(2));
                assert_eq!(messages.string_at("/0/role").as_deref(), Some("user"));
                assert_eq!(messages.array_len("/0/content"), Some(1));
                assert_eq!(
                    messages.string_at("/0/content/0/text").as_deref(),
                    Some("first")
                );
                assert_eq!(messages.string_at("/1/role").as_deref(), Some("user"));
                assert_eq!(messages.array_len("/1/content"), Some(1));
                assert_eq!(
                    messages
                        .payload_at("/1/content/0/text")
                        .unwrap()
                        .root_string_code_units(),
                    Some(expected.to_vec())
                );
            }
        }
    });
}

#[test]
fn raw_runtime_count_estimate_and_generation_ignore_service_environment() {
    run(async {
        let runtime = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile("target", ProtocolFamily::AnthropicMessages, 1000)],
            })
            .unwrap(),
        );
        let mut request = LlmRequest::new(MODEL)
            .with_profile("target")
            .with_user_text("hello");
        request.input.system = vec![sdk::protocol::SystemBlock::new("logical ".repeat(2000))];
        request.input.max_tokens = Some(64);
        let estimate = llm_runtime::model::count_tokens::approximate_tokens(&request);
        assert!(estimate > 1000);
        for extra in [r#"{"system":"tiny"}"#, r#"{"betas":{"toString":null}}"#] {
            std::env::set_var("CLAUDE_CODE_EXTRA_BODY", extra);
            assert_eq!(
                llm_runtime::model::count_tokens::approximate_tokens(&request),
                estimate
            );
            for stream in [false, true] {
                let mut attempt = request.clone();
                attempt.stream = stream;
                let transport = Arc::new(LocalTransport::default());
                let count = runtime
                    .prepare_count_tokens(&attempt)
                    .await
                    .unwrap()
                    .wire_body_bytes()
                    .unwrap();
                if stream {
                    let mut frames = runtime
                        .execute_stream(&attempt, transport.clone())
                        .await
                        .unwrap();
                    while let Some(batch) = frames.next_batch().await {
                        for event in batch.events {
                            event.unwrap();
                        }
                    }
                } else {
                    runtime.execute(&attempt, transport.clone()).await.unwrap();
                }
                let count = sdk::protocol::JsonPayload::parse(std::str::from_utf8(&count).unwrap())
                    .unwrap();
                let calls = transport.calls.lock().unwrap();
                assert_eq!(calls.len(), 1, "readonly count does not dispatch");
                assert!(!calls[0].count);
                assert_eq!(calls[0].body.bool_at("/stream") == Some(true), stream);
                assert_eq!(
                    count.payload_at("/system"),
                    calls[0].body.payload_at("/system")
                );
                assert!(calls[0].body.as_json_source().contains("logical logical"));
            }
        }
    });
}

#[test]
fn exact_count_keeps_effective_canonical_alias_and_kind_format_across_await() {
    run(async {
        let canonical = sdk::protocol::JsonPayload::parse(r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#).unwrap();
        let explicit = r#"{"type":"json_schema","schema":{"type":"object","properties":{"\ud800":{"type":"string","description":"😀"}}}}"#;
        for stream in [false, true] {
            for kind in [AnthropicRequestKind::Main, AnthropicRequestKind::SideQuery] {
                for case in 0..6 {
                    let extra = match case {
                        0 => "{}".to_owned(),
                        1 => r#"{"output_config":{"effort":"high"}}"#.to_owned(),
                        2 => format!(r#"{{"output_config":{{"format":{explicit}}}}}"#),
                        3 => r#"{"output_config":{"format":null}}"#.to_owned(),
                        4 => format!(r#"{{"output_format":{explicit}}}"#),
                        _ => format!(r#"{{"output_config":{{"format":{explicit}}}}}"#),
                    };
                    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", extra);
                    let runtime = Arc::new(
                        ModelRuntime::from_config(ClientConfig {
                            providers: vec![profile(
                                "target",
                                ProtocolFamily::AnthropicMessages,
                                1000,
                            )],
                        })
                        .unwrap(),
                    );
                    let transport = Arc::new(LocalTransport::default());
                    *transport.replace_env_on_count.lock().unwrap() = Some(r#"{"output_config":{"format":{"type":"json_schema","schema":{"type":"boolean"}}}}"#.into());
                    let service = api(runtime, transport.clone());
                    let mut request = LlmRequest::new(MODEL)
                        .with_profile("target")
                        .with_user_text("hello");
                    request.input.system =
                        vec![sdk::protocol::SystemBlock::new("near margin ".repeat(250))];
                    request.input.max_tokens = Some(64);
                    request.execution.input_protocol = Some(ProtocolFamily::AnthropicMessages);
                    request.execution.anthropic_request_kind = kind;
                    if case < 4 {
                        request.input.output_format = sdk::protocol::OutputFormat::JsonSchema {
                            name: "response".into(),
                            schema: canonical.clone(),
                            strict: true,
                        };
                    }
                    execute(&service, request, stream).await;
                    let calls = transport.calls.lock().unwrap();
                    assert_eq!(
                        calls.len(),
                        2,
                        "count then generation: {kind:?}/{case}/{stream}"
                    );
                    assert!(calls[0].count && !calls[1].count);
                    let count = calls[0].body.payload_at("/output_config/format");
                    let generation = calls[1].body.payload_at("/output_config/format");
                    match (&count, &generation) {
                        (Some(count), Some(generation)) => assert!(
                            count.semantically_eq(generation).unwrap(),
                            "effective format: {kind:?}/{case}/{stream}"
                        ),
                        (None, None) => {}
                        _ => panic!("effective format differs: {kind:?}/{case}/{stream}"),
                    }
                    if case == 1 && kind == AnthropicRequestKind::SideQuery {
                        assert!(count.is_none());
                    } else if case == 3 {
                        assert_eq!(count.unwrap().as_json_source(), "null");
                    } else {
                        let format = count.unwrap();
                        assert_eq!(format.string_at("/type").as_deref(), Some("json_schema"));
                        assert_ne!(
                            format.string_at("/schema/type").as_deref(),
                            Some("boolean"),
                            "await must keep original capture"
                        );
                        if case >= 2 {
                            assert!(format.as_json_source().contains(r#""\ud800""#));
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn unsupported_reasoning_keeps_explicit_effort_and_side_disabled_cap_in_both_flows() {
    run(async {
        use sdk::protocol::{ReasoningEffort, ThinkingConfig, ThinkingMode};
        let model = "claude-opus-5";
        let mut provider = profile("native", ProtocolFamily::AnthropicMessages, 1000);
        provider.provider_id = llm_runtime::ProviderId::AnthropicFirstParty;
        provider.base_url = "https://api.anthropic.com".into();
        provider.models[0].display_model = model.into();
        provider.models[0].request_model = model.into();
        provider.models[0].billing_model = model.into();
        provider.models[0].capabilities.reasoning = false;
        let runtime = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![provider],
            })
            .unwrap(),
        );
        for stream in [false, true] {
            for (kind, mode, effort, hook, expected) in [
                (
                    AnthropicRequestKind::Main,
                    ThinkingMode::Enabled,
                    Some(ReasoningEffort::Low),
                    None,
                    "low",
                ),
                (
                    AnthropicRequestKind::Main,
                    ThinkingMode::Disabled,
                    Some(ReasoningEffort::Low),
                    None,
                    "low",
                ),
                (
                    AnthropicRequestKind::SideQuery,
                    ThinkingMode::Enabled,
                    Some(ReasoningEffort::Low),
                    None,
                    "low",
                ),
                (
                    AnthropicRequestKind::SideQuery,
                    ThinkingMode::Disabled,
                    Some(ReasoningEffort::Low),
                    None,
                    "low",
                ),
                (
                    AnthropicRequestKind::SideQuery,
                    ThinkingMode::Enabled,
                    None,
                    Some("max"),
                    "max",
                ),
                (
                    AnthropicRequestKind::SideQuery,
                    ThinkingMode::Disabled,
                    None,
                    Some("max"),
                    "high",
                ),
            ] {
                let mut request = LlmRequest::new(model)
                    .with_profile("native")
                    .with_user_text("hello");
                request.stream = stream;
                request.input.max_tokens = Some(64);
                request.input.thinking = Some(ThinkingConfig {
                    mode: Some(mode),
                    effort,
                    ..Default::default()
                });
                request.execution.resolve_native_effort = true;
                request.execution.anthropic_request_kind = kind;
                request.execution.effort_state.hook = hook.map(|level| json!(level));
                assert!(
                    !request.execution.side_thinking_disabled,
                    "the request's explicit mode must supply the cap"
                );
                let transport = Arc::new(LocalTransport::default());
                let service = api(runtime.clone(), transport.clone());
                execute(&service, request, stream).await;
                let calls = transport.calls.lock().unwrap();
                assert_eq!(calls.len(), 1);
                assert!(!calls[0].count);
                assert_eq!(calls[0].body.bool_at("/stream") == Some(true), stream);
                assert!(
                    calls[0].body.payload_at("/thinking").is_none(),
                    "unsupported reasoning stays absent from the physical input"
                );
                assert_eq!(
                    calls[0].body.string_at("/output_config/effort").as_deref(),
                    Some(expected),
                    "{kind:?}/{mode:?}/{hook:?}/{stream}"
                );
            }
        }
    });
}

#[test]
fn copilot_effective_protocol_preserves_unanchored_input_and_budget_checks_in_both_flows() {
    run(async {
        const BUDGET: u64 = 13_000;
        for model in ["gpt-5.5", "gpt-5-codex"] {
            let mut provider = profile("github-copilot", ProtocolFamily::OpenAiChat, BUDGET);
            provider.provider_id = llm_runtime::ProviderId::OpenAICompatible {
                name: "github-copilot".into(),
            };
            provider.models[0].display_model = model.into();
            provider.models[0].request_model = model.into();
            provider.models[0].billing_model = model.into();
            let runtime = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![provider],
                })
                .unwrap(),
            );
            let native_text = "native Responses model-visible history ".repeat(1000);
            let mut request = LlmRequest::new(model)
                .with_profile("github-copilot")
                .with_user_text("continue");
            request.input.max_tokens = Some(64);
            request.execution.input_protocol = Some(ProtocolFamily::OpenAiResponses);
            request.input.messages.insert(0, sdk::protocol::ConversationMessage {
                role:sdk::protocol::MessageRole::Assistant,
                content:vec![sdk::protocol::ContentBlock::ProviderContent {
                    protocol:ProtocolFamily::OpenAiResponses,
                    value:sdk::protocol::JsonPayload::from_generated_value(&json!({
                        "type":"message", "role":"assistant", "content":[{"type":"output_text", "text":native_text, "annotations":[]}]
                    })).unwrap(),
                }],
                native_options:Vec::new(),
            });
            assert!(request.execution.context_usage_estimate.is_none());
            assert_eq!(
                runtime
                    .protocol_for_model(model, Some("github-copilot"))
                    .unwrap(),
                ProtocolFamily::OpenAiResponses
            );
            let approximate = llm_runtime::model::count_tokens::approximate_tokens(&request);
            assert!(
                approximate > BUDGET && approximate < BUDGET + 1000,
                "full input sits near the configured budget"
            );
            let counter = Arc::new(LocalTransport::default());
            assert!(
                llm_runtime::model::count_tokens::try_count_tokens_exact(
                    &runtime,
                    counter.clone(),
                    &request
                )
                .await
                .unwrap()
                .is_none(),
                "Responses has no exact counter"
            );
            assert_eq!(
                llm_runtime::model::count_tokens::count_tokens(&runtime, counter.clone(), &request)
                    .await
                    .unwrap(),
                approximate,
                "route accounting must retain native Responses content"
            );
            assert!(matches!(
                runtime.prepare_count_tokens(&request).await,
                Err(llm_runtime::LlmError::InvalidRequest { .. })
            ));
            assert!(counter.calls.lock().unwrap().is_empty());
            for stream in [false, true] {
                let mut physical = request.clone();
                physical.stream = stream;
                let transport = Arc::new(LocalTransport::default());
                if stream {
                    let mut frames = runtime
                        .execute_stream(&physical, transport.clone())
                        .await
                        .unwrap();
                    while let Some(batch) = frames.next_batch().await {
                        for event in batch.events {
                            event.unwrap();
                        }
                    }
                } else {
                    runtime.execute(&physical, transport.clone()).await.unwrap();
                }
                {
                    let calls = transport.calls.lock().unwrap();
                    assert_eq!(calls.len(), 1);
                    assert!(calls[0].url.ends_with("/responses"));
                    assert_eq!(calls[0].body.bool_at("/stream") == Some(true), stream);
                    assert_eq!(
                        calls[0]
                            .body
                            .string_at("/input/0/content/0/text")
                            .as_deref(),
                        Some(native_text.as_str())
                    );
                    assert!(calls[0].body.as_json_source().len() > native_text.len());
                }
                let guarded_transport = Arc::new(LocalTransport::default());
                let service = api(runtime.clone(), guarded_transport.clone());
                let result = if stream {
                    async {
                        let mut events = service.stream_request(request.clone()).await?;
                        while let Some(event) = events.next().await {
                            event?;
                        }
                        Ok::<(), llm_runtime::LlmError>(())
                    }
                    .await
                } else {
                    service
                        .execute_non_stream_request(
                            request.clone(),
                            llm_runtime::NonStreamingRequestClass::Main,
                            Default::default(),
                        )
                        .await
                        .map(|_| ())
                };
                assert!(
                    matches!(&result, Err(llm_runtime::LlmError::InputBudgetExceeded { input_tokens, budget:BUDGET, estimated:true }) if *input_tokens == approximate),
                    "{model}/{stream}: {result:?}"
                );
                assert!(
                    guarded_transport.calls.lock().unwrap().is_empty(),
                    "budget rejection must precede generation"
                );
                let mut short = request.clone();
                short.input.messages.remove(0);
                execute(&service, short, stream).await;
                let calls = guarded_transport.calls.lock().unwrap();
                assert_eq!(calls.len(), 1, "short same-route control still dispatches");
                assert!(calls[0].url.ends_with("/responses"));
                assert!(!calls[0].count);
            }
        }
    });
}
