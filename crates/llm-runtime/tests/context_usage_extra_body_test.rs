//! Public API regression for captured physical Anthropic overrides. The local
//! transport bills only the physical system; opaque receipts are not raw text.

use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::host::{ContextMetadata, ContextPolicy, ModelMetadata};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use llm_runtime::context_usage::{ContextUsageEstimator, ContextUsagePrefix};
use llm_runtime::services::sdk;
use llm_runtime::{
    ApiService, ClientConfig, MessagesCreateRequest, ModelRuntime, PromptText, ProviderId,
    SubscriberState, SystemPromptInput,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

const MODEL: &str = "claude-sonnet-4-20250514";
const PROFILE: &str = "extra-body-fixture";
const BUDGET: u64 = 1000;
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvRestore(Option<std::ffi::OsString>);
impl EnvRestore {
    fn new() -> Self {
        let restore = Self(std::env::var_os("CLAUDE_CODE_EXTRA_BODY"));
        std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
        restore
    }
}
impl Drop for EnvRestore {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            std::env::set_var("CLAUDE_CODE_EXTRA_BODY", value)
        } else {
            std::env::remove_var("CLAUDE_CODE_EXTRA_BODY")
        }
    }
}

#[derive(Clone, Debug)]
struct Observation {
    count: bool,
    stream: bool,
    system_len: usize,
    tokens: u64,
    body: sdk::protocol::JsonPayload,
    url: String,
}
#[derive(Default)]
struct FakeTransport {
    observations: Mutex<Vec<Observation>>,
    replace_env_on_count: Mutex<Option<String>>,
    replace_env_on_failure: Mutex<Option<String>>,
    count_unsupported: bool,
}
impl FakeTransport {
    fn observations(&self) -> Vec<Observation> {
        self.observations.lock().unwrap().clone()
    }
    fn clear(&self) {
        self.observations.lock().unwrap().clear();
    }
}

#[async_trait]
impl llm_runtime::Transport for FakeTransport {
    async fn send(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        let body =
            sdk::protocol::JsonPayload::parse(std::str::from_utf8(&request.body).unwrap()).unwrap();
        let system = body.payload_at("/system");
        let system_len = system.as_ref().map_or(0, |system| {
            if let Some(text) = system.root_string_code_units() {
                return text.len();
            }
            (0..system.array_len("").unwrap_or(0))
                .map(|index| {
                    system
                        .array_item(index)
                        .and_then(|block| block.payload_at("/text"))
                        .and_then(|text| text.root_string_code_units())
                        .map_or(0, |text| text.len())
                })
                .sum()
        });
        let tokens = (system_len as u64).div_ceil(3) + 8;
        let count = request.url.contains("/count_tokens");
        let stream = body.bool_at("/stream") == Some(true);
        self.observations.lock().unwrap().push(Observation {
            count,
            stream,
            system_len,
            tokens,
            body,
            url: request.url,
        });
        if count {
            let replacement = self.replace_env_on_count.lock().unwrap().take();
            if let Some(replacement) = replacement {
                std::env::set_var("CLAUDE_CODE_EXTRA_BODY", replacement);
                // The environment changes while the public count awaits.
                tokio::task::yield_now().await;
            }
        } else if let Some(replacement) = self.replace_env_on_failure.lock().unwrap().take() {
            std::env::set_var("CLAUDE_CODE_EXTRA_BODY", replacement);
            return response(
                503,
                "application/json",
                serde_json::to_vec(
                    &json!({"error":{"type":"overloaded_error","message":"try next connection"}}),
                )
                .unwrap(),
            );
        }
        if count && self.count_unsupported {
            return response(
                404,
                "application/json",
                serde_json::to_vec(
                    &json!({"error":{"type":"not_found_error","message":"no count endpoint"}}),
                )
                .unwrap(),
            );
        }
        if count {
            return response(
                200,
                "application/json",
                serde_json::to_vec(&json!({"input_tokens":tokens})).unwrap(),
            );
        }
        if stream {
            let frames = [
                json!({"type":"message_start","message":{"id":"fixture-response","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":tokens,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
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
            return response(200, "text/event-stream", source.into_bytes());
        }
        response(200, "application/json", serde_json::to_vec(&json!({
            "id":"fixture-response", "type":"message", "role":"assistant", "model":MODEL,
            "content":[{"type":"text","text":"ok"}], "stop_reason":"end_turn", "stop_sequence":null,
            "usage":{"input_tokens":tokens,"output_tokens":2}
        })).unwrap())
    }
}
fn response(
    status: u16,
    content_type: &str,
    bytes: Vec<u8>,
) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
    Ok(sdk::StreamResponse {
        status,
        headers: vec![("content-type".into(), content_type.into())],
        body: futures::stream::once(async move { Ok(bytes.into()) }).boxed(),
    })
}

fn runtime(failover: bool) -> Arc<ModelRuntime> {
    let mut profile = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "anthropic")
        .unwrap();
    profile.profile_name = PROFILE.into();
    profile.provider_id = ProviderId::Custom {
        name: PROFILE.into(),
    };
    profile.base_url = "https://primary.example.invalid".into();
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
    model.metadata = ModelMetadata {
        context_window_tokens: Some(1_000_000),
        context: Some(ContextMetadata {
            application: Some(ContextPolicy::Custom {
                input_tokens: BUDGET,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let providers = if failover {
        profile.connection.group = Some("extra-body-connections".into());
        profile.connection.connection_id = Some("primary".into());
        profile.connection.failover = llm_runtime::FailoverTriggers::DEFAULT;
        let mut next = profile.clone();
        next.profile_name = "extra-body-secondary".into();
        next.base_url = "https://secondary.example.invalid".into();
        next.connection.connection_id = Some("secondary".into());
        next.connection.order = 1;
        vec![profile, next]
    } else {
        vec![profile]
    };
    Arc::new(ModelRuntime::from_config(ClientConfig { providers }).unwrap())
}
fn service(transport: Arc<FakeTransport>, failover: bool) -> ApiService {
    ApiService::new(
        runtime(failover),
        transport,
        SubscriberState::default(),
        llm_runtime::model::user_agent::UserAgentEnv::default(),
        "fixture",
        None,
        None,
    )
    .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled)
}
fn request(system: &str, history: Vec<ConversationMessage>) -> MessagesCreateRequest {
    let mut request = MessagesCreateRequest::new(
        MODEL,
        Some(PROFILE),
        Some(SystemPromptInput::custom_prompt(PromptText::from_string(
            system,
        ))),
        history,
        vec![],
    );
    request.opts.max_output_tokens = Some(64);
    request
}
fn opaque_history() -> Vec<ConversationMessage> {
    vec![
        ConversationMessage::user(MessageId::new(), "hello".into()),
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: Utf16JsonProjection::from_serde_value(
                    &json!({"type":"redacted_thinking","data":"A".repeat(60_000)}),
                )
                .unwrap(),
            }],
            stop_reason: Some("end_turn".into()),
            per_turn_effort: None,
            resumable: None,
        },
        ConversationMessage::user(MessageId::new(), "continue".into()),
    ]
}
async fn create(
    api: &ApiService,
    stream: bool,
    system: &str,
    history: Vec<ConversationMessage>,
) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
    if stream {
        api.messages_create_buffered_stream(request(system, history))
            .await
    } else {
        api.messages_create(request(system, history)).await
    }
}
fn prefix(response: &llm_runtime::HistoryResponse) -> ContextUsagePrefix {
    serde_json::from_str(
        response
            .usage
            .provider_metadata
            .payload_at("/lingxi_context_usage_prefix")
            .unwrap()
            .as_json_source(),
    )
    .unwrap()
}
fn estimator(response: &llm_runtime::HistoryResponse) -> ContextUsageEstimator {
    let prefix = prefix(response);
    let tokens = response.usage.context_tokens().unwrap();
    ContextUsageEstimator::with_prefix(move |_, _, history| {
        prefix
            .matches_history(history)
            .then(|| (tokens, prefix.clone()))
    })
}
fn extra_system(text: &str) -> String {
    json!({"system":[{"type":"text","text":text}]}).to_string()
}
fn run(test: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let _lock = ENV_LOCK.lock().unwrap();
            let _restore = EnvRestore::new();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(test);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn physical_extra_system_invalidates_measured_prefix_for_stream_and_nonstream() {
    run(async {
        for stream in [false, true] {
            std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
            let big = "large physical system ".repeat(6000);
            let transport = Arc::new(FakeTransport::default());
            let api = service(transport.clone(), false);
            let history = opaque_history();
            let control = create(&api, stream, &big, history.clone()).await;
            assert!(matches!(
                control,
                Err(llm_runtime::LlmError::InputBudgetExceeded { .. })
            ));
            assert!(transport
                .observations()
                .iter()
                .all(|observation| observation.count));
            transport.clear();
            let first = create(&api, stream, "stable logical system", history.clone())
                .await
                .unwrap();
            let first_prefix = prefix(&first);
            assert!(transport
                .observations()
                .iter()
                .any(|observation| observation.count));
            assert!(transport
                .observations()
                .iter()
                .any(|observation| !observation.count && observation.stream == stream));
            transport.clear();
            // New metadata/output caps must keep the measured opaque prefix.
            std::env::set_var(
                "CLAUDE_CODE_EXTRA_BODY",
                r#"{"metadata":{"fixture":"new"},"max_tokens":65}"#,
            );
            let mut output_retry = request("stable logical system", history.clone());
            output_retry.opts.max_output_tokens = Some(65);
            let same = estimator(&first)
                .scope(async {
                    if stream {
                        api.messages_create_buffered_stream(output_retry).await
                    } else {
                        api.messages_create(output_retry).await
                    }
                })
                .await
                .unwrap();
            assert_eq!(prefix(&same), first_prefix);
            assert_eq!(transport.observations().len(), 1);
            assert!(!transport.observations()[0].count);
            transport.clear();
            std::env::set_var("CLAUDE_CODE_EXTRA_BODY", extra_system(&big));
            let exact = api
                .count_tokens_exact(
                    MODEL,
                    Some(PROFILE),
                    Some("stable logical system"),
                    history.clone(),
                    vec![],
                )
                .await
                .unwrap()
                .unwrap();
            assert!(exact > BUDGET);
            assert_eq!(transport.observations()[0].system_len, big.len());
            let facade = api
                .count_tokens(
                    MODEL,
                    Some(PROFILE),
                    Some("stable logical system"),
                    history.clone(),
                    vec![],
                )
                .await
                .unwrap();
            assert_eq!(facade, exact);
            transport.clear();
            let second = estimator(&first)
                .scope(create(&api, stream, "stable logical system", history))
                .await;
            assert!(matches!(
                second,
                Err(llm_runtime::LlmError::InputBudgetExceeded { .. })
            ));
            let calls = transport.observations();
            assert!(!calls.is_empty());
            assert!(calls
                .iter()
                .all(|call| call.count && call.system_len == big.len() && call.tokens > BUDGET));
        }
    });
}

#[test]
fn count_and_generation_share_extra_snapshot_across_await_and_connection_retry() {
    run(async {
        for stream in [false, true] {
            for retry in [false, true] {
                let captured = "captured physical system";
                let later = extra_system(&"new operation only ".repeat(6000));
                std::env::set_var("CLAUDE_CODE_EXTRA_BODY", extra_system(captured));
                let transport = Arc::new(FakeTransport::default());
                if retry {
                    *transport.replace_env_on_failure.lock().unwrap() = Some(later);
                } else {
                    *transport.replace_env_on_count.lock().unwrap() = Some(later);
                }
                let api = service(transport.clone(), retry);
                let result = create(&api, stream, "logical system", opaque_history())
                    .await
                    .unwrap();
                let calls = transport.observations();
                assert!(calls.iter().any(|call| call.count));
                assert!(calls.iter().any(|call| !call.count));
                assert!(calls.iter().all(|call| call.system_len == captured.len()));
                assert_eq!(
                    result.usage.context_tokens(),
                    Some((captured.len() as u64).div_ceil(3) + 10)
                );
                if retry {
                    assert!(calls
                        .iter()
                        .any(|call| call.url.starts_with("https://secondary.example.invalid")));
                }
            }
        }
    });
}

#[test]
fn physical_overrides_reach_readonly_count_and_generation_without_losing_exact_strings() {
    run(async {
        for stream in [false, true] {
            // Both modes preserve paired text and exact schema keys. Native
            // generation intentionally repairs lone surrogate values, whereas
            // CountTokens retains them; that existing SDK policy is separate.
            std::env::set_var(
                "CLAUDE_CODE_EXTRA_BODY",
                r#"{"system":[{"type":"text","text":"\ud83d\ude00"}],"messages":[{"role":"user","content":"\ud83d\ude80"}],"tools":[{"name":"f","description":"fixture","input_schema":{"type":"object","properties":{"\ud800":{"type":"string","description":"\ud83d\ude00"}}}}],"metadata":{"fixture":"ignored by count"},"max_tokens":64}"#,
            );
            let transport = Arc::new(FakeTransport::default());
            let api = service(transport.clone(), false);
            let captured = api
                .build_scheduled_request(
                    request("logical", opaque_history()),
                    llm_runtime::model::thinking::ThinkingConfig::Disabled,
                    None,
                )
                .unwrap();
            let read_only = runtime(false)
                .prepare_count_tokens(&captured)
                .await
                .unwrap();
            // Exact JavaScript strings can live in the prepared request's
            // string sidecar. Compare the actual wire projection, as the
            // transport does, rather than its display-only body value.
            let read_only = sdk::protocol::JsonPayload::parse(
                std::str::from_utf8(&read_only.wire_body_bytes().unwrap()).unwrap(),
            )
            .unwrap();
            api.count_tokens_exact(
                MODEL,
                Some(PROFILE),
                Some("logical"),
                opaque_history(),
                vec![],
            )
            .await
            .unwrap()
            .unwrap();
            create(&api, stream, "logical", opaque_history())
                .await
                .unwrap();
            let calls = transport.observations();
            let count = calls.iter().find(|call| call.count).unwrap();
            let generation = calls.iter().find(|call| !call.count).unwrap();
            for field in ["/system", "/messages", "/tools"] {
                assert!(
                    read_only
                        .payload_at(field)
                        .unwrap()
                        .semantically_eq(&generation.body.payload_at(field).unwrap())
                        .unwrap(),
                    "read-only prepare: {field}"
                );
                assert!(
                    count
                        .body
                        .payload_at(field)
                        .unwrap()
                        .semantically_eq(&generation.body.payload_at(field).unwrap())
                        .unwrap(),
                    "{field}"
                );
            }
            assert_eq!(
                count
                    .body
                    .payload_at("/system/0/text")
                    .unwrap()
                    .root_string_code_units(),
                Some(vec![0xd83d, 0xde00])
            );
            assert_eq!(
                generation
                    .body
                    .payload_at("/messages/0/content")
                    .unwrap()
                    .root_string_code_units(),
                Some(vec![0xd83d, 0xde80])
            );
            for body in [&read_only, &count.body, &generation.body] {
                assert!(
                    body.payload_at("/tools/0/input_schema/properties")
                        .unwrap()
                        .as_json_source()
                        .contains(r#""\ud800""#),
                    "native normalization must retain the exact schema key"
                );
            }
            assert!(count.body.payload_at("/max_tokens").is_none());
            assert!(count.body.payload_at("/metadata").is_none());
        }
    });
}

#[test]
fn unsupported_counter_falls_back_to_physical_extra_input_and_responses_ignore_invalid_env() {
    run(async {
        let big = "large fallback system ".repeat(6000);
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", extra_system(&big));
        let raw = llm_runtime::LlmRequest::new(MODEL).with_user_text("hi");
        assert!(llm_runtime::model::count_tokens::approximate_tokens(&raw) < BUDGET);
        assert!(llm_runtime::model::count_tokens::estimate_input_tokens(&raw) < BUDGET);
        let transport = Arc::new(FakeTransport {
            count_unsupported: true,
            ..Default::default()
        });
        let api = service(transport, false);
        let approximate = api
            .count_tokens(
                MODEL,
                Some(PROFILE),
                Some("small"),
                vec![ConversationMessage::user(MessageId::new(), "hi".into())],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            approximate > BUDGET,
            "unsupported endpoint must not estimate the small canonical system"
        );

        let config: ClientConfig = serde_json::from_value(json!({"providers":[{
            "provider_id":{"custom":{"name":"responses-fixture"}}, "profile_name":"responses-fixture",
            "base_url":"https://example.invalid/v1", "protocol":"open_ai_responses",
            "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"m","request_model":"m","billing_model":"m"}]
        }]})).unwrap();
        let runtime = Arc::new(ModelRuntime::from_config(config).unwrap());
        let request = llm_runtime::LlmRequest::new("m")
            .with_profile("responses-fixture")
            .with_user_text("hi");
        std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
        let normal = runtime
            .prepare(&request)
            .await
            .unwrap()
            .provider_request
            .wire_body_bytes()
            .unwrap();
        let service = ApiService::new(
            runtime.clone(),
            Arc::new(FakeTransport::default()),
            SubscriberState::default(),
            llm_runtime::model::user_agent::UserAgentEnv::default(),
            "fixture",
            None,
            None,
        );
        let control = service
            .count_tokens(
                "m",
                Some("responses-fixture"),
                Some("system"),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        for invalid in ["not-json", "[]", r#"{"messages":17}"#] {
            std::env::set_var("CLAUDE_CODE_EXTRA_BODY", invalid);
            assert_eq!(
                runtime
                    .prepare(&request)
                    .await
                    .unwrap()
                    .provider_request
                    .wire_body_bytes()
                    .unwrap(),
                normal
            );
            assert_eq!(
                service
                    .count_tokens(
                        "m",
                        Some("responses-fixture"),
                        Some("system"),
                        vec![],
                        vec![]
                    )
                    .await
                    .unwrap(),
                control
            );
        }
    });
}
